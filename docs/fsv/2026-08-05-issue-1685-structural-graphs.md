# Issue #1685: Structural graph and hierarchy lenses

Date: 2026-08-05

## Source of truth

- Production vault: `%LOCALAPPDATA%\synapse\db-daemon`, vault id
  `01KYJPGWATPD4XNMZY3ERGTKQW`.
- Durable evidence: Aster `Registry`, `Graph`, `Base`, and per-slot CF rows.
- Runtime evidence: installed `synapse-mcp.exe`, OS process table/listening socket,
  scheduled-maintenance log, and a fresh HTTP MCP `tools/call`.
- Synthetic evidence: new temporary Aster vaults populated through the real storage
  and snapshot publication paths. Temporary vaults were independently inspected and
  deleted after the readback.

## Diagnosis and research

The original system had no scheduled structural publisher. The first implementation
then exposed three deeper defects during manual verification:

1. The process panel read agent spawn events but not `CF_PROCESS_HISTORY`.
2. A changed snapshot attempted to replace frozen lenses under the same lifecycle
   row and failed with `SYNAPSE_CALYX_PANEL_LIFECYCLE_CONFLICT`.
3. On a fresh vault, the dynamic allocator consumed `1685003` before the fixed path
   panel had reserved it.

The final design reads each source at a coherent snapshot, prefixes heterogeneous
node identities, allocates a new generation for every changed fingerprint, guards
Registry replacement by the exact SHA-256 revision inside the same atomic
constellation/ledger/Graph/Registry commit, and reserves all fixed generations in
the derived family before allocating any dynamic generation.

Research lanes: Exa MCP was live (real `web_search_exa` call) and the built-in web
lane was also used. Primary guidance applied:

- [Apache Flink state schema evolution](https://nightlies.apache.org/flink/flink-docs-stable/docs/dev/datastream/fault-tolerance/serialization/schema_evolution/): persisted state is read with its prior schema and migrated under an explicitly compatible serializer contract.
- [Neo4j transaction management](https://neo4j.com/docs/java-reference/current/transaction-management/): related graph mutations are committed as one transaction rather than exposing partial state.

## Synthetic full-state verification

Known app graph: `leaf-a -> hub`, `leaf-b -> hub`, `hub -> leaf-c`,
`hub -> leaf-d`.

- Before: `(Graph, Registry, Base) = (0, 0, 0)`.
- Trigger: real `publish_graph_position_snapshot`.
- After: `(5, 2, 5)`; hub betweenness was exactly `1/3`, every leaf was `0`,
  and the hub dense signature differed from the leaves.
- Idempotent replay: counts and committed sequence did not change.
- Changed snapshot: adding `leaf-d -> leaf-e` advanced generation `1685004 ->
  1685005`; physical counts advanced `(5,2,5) -> (11,2,11)`.
- Path publication after both graph generations succeeded at `1685006`, proving
  the fixed built-in generation `1685003` was not stolen.

Known process tree: `1 -> 10`, `10 -> 20`, `10 -> 30` using authoritative integer
fields `parent_pid` and `ppid`.

- Before: zero `CF_PROCESS_HISTORY` rows and no process-graph lifecycle.
- Trigger: three real CF writes followed by the real scheduled-maintenance entry.
- After: all three source JSON rows were reread byte-for-byte; lifecycle panel
  `1685002` contained two frozen snapshot lenses with snapshot
  `276972968554941791`.

Boundary audit (state printed before and after):

1. Empty transition list: refused; `(0,0,0) -> (0,0,0)`.
2. Zero-count edge: refused; `(0,0,0) -> (0,0,0)`.
3. Exact replay: accepted idempotently; `(5,2,5) -> (5,2,5)`.

The process-tree run also exposed timestamp-absent temporal backfill defect #2013;
it was filed with the exact failure instead of being hidden by altered data.

## Production execution and independent readback

Commit `fc41d04f` was deployed through `scripts/synapse-setup.ps1`. Installed and
release artifacts matched; installed SHA-256:
`F228BE409AD4E5B8751D8050503B5B4B873354292E3CAEC1B8B598653F641ACD`.
PID `22972` owned `127.0.0.1:7700` after setup.

Scheduled publisher log:

```text
panel_version=2006016 source_seq=651746 snapshot=16337785314158783495
constellation_count=110 graph_row_count=57 committed_seq=651748
lifecycle_sha256=2fc64bb12193bff1ddc688f0b86a644b3675549854794e6716a25cbdeb36079b
```

Separate read-only `dump_cf --slot-kind-census` at vault snapshot `652070`:

```text
panel_rows=110 undecodable_base=0 declared_slots=2
slot=98 dense=110 absent=0 missing_cf_row=0 undecodable=0 dims=[8] distinct_values=4
slot=99 sparse=110 absent=0 missing_cf_row=0 undecodable=0 dims=[2048] observed_support=106 distinct_values=107
```

Fresh HTTP MCP session, real `storage operation=intelligence` / `abundance` call:

```text
panel=2006016 n_constellations=110 n_lenses=2 measurable_lenses=2
measured_count=220 c_n2=1 materialized=3407 dda_signal_yield=440
slot 98 dense measurable=true; slot 99 sparse measurable=true support=106
source_of_truth="Calyx Base + XTerm + Graph CF rows"
```

## Build gates

`cargo check --workspace` passed. `pwsh -File scripts/lint.ps1` passed all seven
gates in both the root and Calyx workspaces, including the unreached-public-API
ratchet remaining at 363.
