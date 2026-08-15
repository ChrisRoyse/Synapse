# Issue #2249 — exhaustive typed causal-evidence maps

Date: 2026-08-15

Commits under verification:

- `d2dd01defd01f9b74436aa1234ed39e190b31244` — production causal-map surface and exhaustive typed evidence
- `11cf1ababdbdfcbf571ca68dbdc4f306151b0fde` — native Calyx-CF snapshot readback
- `9aca3d1d147b0ebdd49dbf44b24f73ce96a586ed` — deterministic causal-artifact projection

Method: manual FSV only. No test, benchmark, harness, script, mock row, direct database write, or CI job was used as behavioral evidence. The behavioral trigger was the real strict-client `mcp__synapse__storage` tool; shell reads only inspected independent physical Sources of Truth.

## Source of Truth

- Runtime identity: the installed `synapse-mcp.exe` process and listening socket at `127.0.0.1:7700`.
- Production trigger: `storage operation=intelligence`, nested `operation=causal_map`, through the wired Codex MCP client after strict validation of all 40 `tools/list` schemas.
- Source intelligence: 16 retained, real MCP usage constellations in panel `1965007`, split into the known streams `health=8` and `storage=8` inside the declared source-event window.
- Durable output: the exact content-addressed `GCMP1` row in native Calyx `Graph`, addressed by the returned key and read through a new Aster MVCC snapshot.
- Refusal/no-write state: the durable daemon log count of `SYNAPSE_CALYX_CAUSAL_MAP_PERSISTED_AND_READ_BACK`; every invalid trigger was bracketed by a separate count read.
- Search-generation prerequisite: `C:\Users\hotra\AppData\Local\synapse\db-daemon\idx\search\panel_0001965007\manifest.json`.

A tool return value is not the verdict. Acceptance requires the process/socket read, strict schema discovery, independent manifest read, exact native-CF point read, repeated content-address comparison, and refusal log counts below.

## Root causes and resulting invariants

The original reachable Synapse behavior exposed only one transfer-entropy pair. Omitting pair values silently chose the two most frequent groups; the shared loader ignored public time bounds and could stop on an arbitrary panel-membership prefix. A non-provisional predictive estimate was called grounded even though observational event timing did not identify an intervention effect.

The first implementation FSV then found two additional physical defects:

1. `snapshot_read` always encoded its key into Synapse's logical `Kv` envelope, so it could not separately read the native `Graph` row which the causal operation claimed to persist.
2. Transfer-entropy `computed_at` wall-clock values were serialized into the content-addressed artifact. Two identical calls therefore produced different SHA-256 digests and leaked a new Graph row per invocation.

The shipped invariants are:

- neither pair named means exhaustive `C(n,2)`; both named means exactly one pair; one-sided selection fails;
- the entire requested time scope must fit `max_records`; the first excess record fails instead of sampling;
- PC-stable, partial correlation, transfer entropy, bidirectional Granger, signed lag correlation, CCM, temporal cross-K, Hawkes branching, and Benjamini-Hochberg families remain typed and separate;
- observational estimator agreement never upgrades `observational_predictive` to an identified structural effect;
- runtime-only estimator timestamps are excluded from semantic artifact bytes, source-data time remains explicit, and projection-shape drift fails closed;
- the native Graph write is flushed, byte-compared internally, then independently addressable through `snapshot_read cf_name=Graph`.

The implementation was informed by primary-source research performed through both Exa MCP and native web research:

- <https://jmlr.org/papers/v8/kalisch07a.html>
- <https://link.aps.org/doi/10.1103/PhysRevLett.103.238701>
- <https://proceedings.mlr.press/v48/xuc16.html>
- <https://arxiv.org/abs/1601.01879>
- <https://www.science.org/doi/10.1126/science.1227079>
- <https://www.jstor.org/stable/2346101>
- <https://www.hsph.harvard.edu/miguel-hernan/causal-inference-book/>

The binding interpretation and assumptions are recorded in `docs/adr/2026-08-15-exhaustive-typed-causal-evidence-maps.md`.

## Installed runtime precondition

Separate host and MCP reads after installation showed:

```text
daemon PID:       57836
parent PID:       31000
executable:       C:\Users\hotra\.cargo\bin\synapse-mcp.exe
command line:     --mode http --bind 127.0.0.1:7700 --db ...\db-daemon
socket:           127.0.0.1:7700 LISTEN, owning PID 57836
health build:     9aca3d1d147b
checkout commit:  9aca3d1d147b0ebdd49dbf44b24f73ce96a586ed
build tree:       clean, changed input count 0
advertised tools: 40
strict client:    40 mcp__synapse__ tools loaded; health and storage present
tool surface:     14b4f2f39de7e98e27be249c83c7fc7239c48a679c07186585410973092508cf
installed length: 262068041 bytes
installed SHA256: 419242AA64A443261B6063401D8DA96D33E52F5E27BBF43D9C75642B7150A2A1
```

The standard setup command built and handed off this daemon, then returned nonzero at the separate pre-existing Chrome bridge checkpoint:

```text
SYNAPSE_CHROME_BRIDGE_ACTIVATION_PENDING
reload_self_capability=false
checkpoint=%LOCALAPPDATA%\synapse\setup-chrome-bridge-pending.json
```

Setup itself is therefore not reported as successful. The daemon handoff was independently proven by the process, socket, executable hash, build provenance, health, and strict tool discovery above. No human Chrome window was touched.

## Recovered-generation prerequisite

The first post-restart causal trigger failed closed:

```text
SYNAPSE_CALYX_STALE_DERIVED
changed-key history for Base begins at recovered seq 4113856
requested generation delta began after seq 4112273
```

Through the public maintenance-gated surface, the agent acquired the foreground lease, selected `break_glass`, ran `storage search_rebuild expected_panel_version=1965007`, restored `normal_agent`, and released the lease. A separate filesystem read proved:

```text
manifest:      C:\Users\hotra\AppData\Local\synapse\db-daemon\idx\search\panel_0001965007\manifest.json
length:        4219 bytes
panel_version: 1965007
base_seq:      4114484
slot_count:    11
SHA256:        441D86708202B0775741DDADC419A2F7524F9380E4678BE7FA4B47B80E22E918
```

The SHA matched the MCP rebuild readback exactly. The final lease state was `held=false`.

## Happy path: exhaustive map and deterministic row reuse

The production trigger used:

```text
panel_version=1965007
group_key=mcp_usage_tool
since_ts_ns=1786833342023000000 inclusive
until_ts_ns=1786833432141000000 exclusive
max_records=32
bin_seconds=0.05
max_lag=4
causal_fdr_alpha=0.05
group_a/group_b omitted => exhaustive C(n,2)
```

The first call read every requested source row and returned:

```text
source_records=16
streams=[health:8, storage:8]
expected_pair_count=1
actual_pair_count=1
all_requested_records_loaded=true
all_stream_pairs_enumerated=true
evidence_class=observational_predictive
structural_effect_identified=false
graph_key=47434d5031001dfbcfce42b5e6015f997ad279c77db9700a30
graph_value_sha256=ce42b5e6015f997ad279c77db9700a30935b94cb2b461d9fe98ec573c3738664
graph_value_bytes=13525
graph_cf_rows_after=7913416
physical_readback_matches=true
transfer_entropy computed_at fields persisted=0
```

The FDR families physically returned `8` Granger hypotheses, `9` cross-correlation hypotheses, `1` PC edge-removal hypothesis, and `0` partial-network hypotheses for this two-stream scope.

Estimator non-applicability was preserved rather than covered up: discrete TE remained typed `unresolved` because strict CUDA has no discrete plug-in kernel, CCM reported degenerate constant targets, and a two-variable partial-correlation network reported insufficient variables. Granger, signed cross-correlation, PC skeleton, cross-K, and Hawkes returned their own measured results. No lane substituted for another, and no structural-effect claim was manufactured.

A separately opened MVCC snapshot then read the exact native row:

```text
cf_name=Graph
key_hex=47434d5031001dfbcfce42b5e6015f997ad279c77db9700a30
physical_present=true
logical_present=true
payload_len_bytes=13525
payload_sha256=sha256:ce42b5e6015f997ad279c77db9700a30935b94cb2b461d9fe98ec573c3738664
```

The identical production trigger was issued a second time. Its independently returned state was byte-identical:

```text
key_equal=true
sha_equal=true
bytes_equal=true
graph_rows_equal=true
graph_key=47434d5031001dfbcfce42b5e6015f997ad279c77db9700a30
SHA256=ce42b5e6015f997ad279c77db9700a30935b94cb2b461d9fe98ec573c3738664
bytes=13525
graph rows=7913416
```

This is the determinism proof: the same source rows and parameters reused one physical content-addressed row instead of appending another. A second fresh snapshot after the repeat, and a third snapshot after all edge cases, both returned the same key, length, and SHA. Every reader lease was explicitly released; final active lease count was zero.

## Edge-case audit

For each case, the physical before/after state was the count of durable `SYNAPSE_CALYX_CAUSAL_MAP_PERSISTED_AND_READ_BACK` records across the daemon logs. The count was `4` before every trigger and `4` afterward, proving that no rejected request completed or published a causal artifact. The final exact Graph point read still matched the happy-path SHA.

### 1. Empty source-event window

```text
before persisted_count=4
trigger since_ts_ns=0 until_ts_ns=1
error SYNAPSE_CALYX_CAUSAL_MAP_EMPTY_SCOPE
after persisted_count=4
```

### 2. Equal inclusive/exclusive boundary

```text
before persisted_count=4
trigger since_ts_ns=1 until_ts_ns=1
error SYNAPSE_CALYX_INTELLIGENCE_TIME_RANGE_INVALID
after persisted_count=4
```

### 3. Structurally incomplete pair selection

```text
before persisted_count=4
trigger group_a=health, group_b omitted
error SYNAPSE_CALYX_CAUSAL_MAP_PAIR_SCOPE_INCOMPLETE
after persisted_count=4
```

The remediation explicitly requires both groups or neither; the engine does not silently change scope.

### 4. Invalid FDR boundary

```text
before persisted_count=4
trigger causal_fdr_alpha=0
error TOOL_PARAMS_INVALID
remediation: finite alpha strictly inside (0,1); validation stopped before storage
after persisted_count=4
```

### 5. Complete scope exceeds the declared record bound

```text
before persisted_count=4
known matching source rows=16
trigger max_records=15
error SYNAPSE_CALYX_TEMPORAL_SCOPE_EXCEEDS_MAX_RECORDS
after persisted_count=4
```

The returned remediation says to narrow the source-event window or raise the bound; it explicitly refuses a biased membership prefix.

Final post-edge physical state:

```text
cf_name=Graph
physical_present=true
logical_present=true
payload_len_bytes=13525
payload_sha256=sha256:ce42b5e6015f997ad279c77db9700a30935b94cb2b461d9fe98ec573c3738664
reader lease released=true
active leases=0
```

## Structural gates

The permitted non-behavioral gates passed:

```text
cargo fmt --all --check
cargo check -p synapse-calyx -p synapse-storage -p synapse-mcp
cargo clippy --workspace --all-targets -- -D warnings
```

No `cargo test`, automated test target, benchmark, FSV driver, CI workflow, or mock fixture was run or added.

## Verdict

PASS for #2249. The real strict MCP client now exposes an exhaustive, bounded, typed causal-evidence map; the full requested source scope is either loaded or refused; observational evidence remains honest about identification; the native Graph artifact is independently readable; identical inputs are byte-deterministic; and all five invalid/boundary cases leave the durable causal-artifact state unchanged.
