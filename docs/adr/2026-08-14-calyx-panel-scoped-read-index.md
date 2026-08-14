# ADR: Panel-scoped Calyx reads use sealed membership indexes

## Status

Accepted — 2026-08-14 (#2245).

## Context

`Base` is globally ordered by content-addressed `CxId`, not by panel. Several
panel-local intelligence paths paged the complete multi-panel column family and
discarded rows from other panels after decoding them. Request bounds limited
matching output, not work. On the live vault, finding 1,000 rows for an
8,846-row panel therefore traversed a multi-million-row global corpus and
exceeded the MCP call boundary.

## Decision

The hash-sealed search filter generation is the durable panel-membership index.
Panel-local readers must:

1. pin one atomic Aster snapshot and its panel content watermark;
2. open and validate the panel's sealed search generation and filter sidecar;
3. prove the generation base sequence is fresh for that exact panel;
4. walk the sidecar's strictly ordered `CxId` membership and point-read those
   exact `Base` identities through the same snapshot;
5. decode and prove both the stored identity and panel version before accepting
   each row.

Missing, stale, malformed, hash-mismatched, out-of-order, duplicate, absent, or
cross-panel membership fails closed. A global-scan fallback is prohibited.
Moving an existing Base row between panels advances both panels' content
watermarks so neither old membership generation can remain apparently fresh.

Global lineage and census operations may still scan globally when their stated
contract is explicitly multi-panel.

## Consequences

- Panel request limits now bound both accepted rows and Base point reads.
- The generation seal and panel watermark make selectivity a verified storage
  invariant rather than a cache assumption.
- Search, temporal intelligence, grounding, kernel maintenance, Ward, lifecycle,
  and action validation share one panel-membership law.

## Research basis

- [RocksDB prefix seek](https://github.com/facebook/rocksdb/wiki/Prefix-Seek): selective reads need a key/index structure whose prefix represents the requested partition.
- [RocksDB options](https://github.com/facebook/rocksdb/blob/main/include/rocksdb/options.h): prefix extractors and bloom/index configuration must agree with the physical key contract.
- [PostgreSQL multicolumn indexes](https://www.postgresql.org/docs/16/indexes-multicolumn.html): leading equality constraints are what bound the portion of an ordered index that must be scanned.
