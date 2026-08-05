# Issue #1681: persisted search quantization FSV (2026-08-04)

## Diagnosis

Persisted search rebuilt dense lanes as either a small exact flat index or a
DiskANN graph, but it never enabled the DiskANN product-quantized candidate
sidecar. Query open explicitly set `rescore_from_raw=false`. Registry slot
compression therefore had no load-bearing relationship to persisted recall.

The first implementation attempt exposed a second root cause against a real
timeline panel: a global bit width tried to train PQ on every dense lane. One
lane was bit-identical across the corpus and the trainer correctly failed with
`CALYX_INDEX_INVALID_PARAMS: pq training corpus is degenerate`. Quantization is
a slot property. The durable configuration is consequently
`quant_bits_by_slot`; an unlisted lane is explicitly exact, while a listed lane
must build its requested 4- or 8-bit artifact or the generation is not
published.

## Research

Research was performed after diagnosis through both configured lanes. The Exa
MCP lane was verified with `scripts/check-research-lane.ps1`; built-in web
research used primary/upstream sources.

- Faiss documents the standard two-stage design: compressed codes select
  candidates and a refinement index reorders them using more accurate vectors:
  <https://github.com/facebookresearch/faiss/wiki/Implementation-notes>
- Faiss describes PQ as a lossy compressed index and recommends measuring the
  speed/accuracy tradeoff for the actual dataset:
  <https://github.com/facebookresearch/faiss/wiki/Faiss-indexes>
- Faiss index-selection guidance treats recall and memory as measured design
  constraints rather than assumed properties:
  <https://github.com/facebookresearch/faiss/wiki/Guidelines-to-choose-an-index>
- Elastic's current quantization guidance likewise retains full-precision
  vectors for rescoring and calibrates quantization against observed data:
  <https://www.elastic.co/search-labs/blog/vector-quantization-auto-calibration-elasticsearch>

The implementation follows that evidence: PQ is candidate-only, the final
ordering is exact cosine from a packed raw sidecar, and both sidecars are
SHA-256-bound into the immutable generation manifest.

## Source of truth

The manual driver is
`crates/synapse-storage/examples/quantized_search_fsv.rs`. It created a real
Aster vault at:

`%TEMP%\synapse-fsv-1681-593aacb1633b42cebfa7aaeef3c49ad0`

The independent read surface was:

- Aster vault sequence and stored slot-104 vectors;
- `idx/search/panel_0001963001/manifest.json`;
- the manifest-named `graph.pq` and `graph.raw` files;
- production `PersistedSearchIndexes::open().search()` after each disk mutation.

## Happy path and recall

The driver ingested 24 deterministic real timeline constellations (`seq=24`),
selected only graded dense slot 104 for 4-bit PQ, rebuilt the real persisted
generation, then separately read the files:

```text
manifest sha256=96dcd4154359234a5490824ae2fff08a39aa4072283291534335de581aefb012
slot=104 bits=4
graph.pq  bytes=2472 sha256=1d9a09c36eea77243451aef9d226724e5f3f1797258ee170ab005f45a153d488
graph.raw bytes=3136 sha256=59fd7813416f8c783adb8b82db674c78c548695ef5dc7163e130f9d450178a19
known query expected=16810800000000000000000000000000 first=16810800000000000000000000000000
```

For every stored vector, the driver independently computed scalar exact cosine
against every other stored vector and compared its exact top 3 with production
PQ-candidate plus raw-rerank search. Result:

```text
RECALL aggregate=72/72 recall=1.000000
```

## Boundary audit

Each case printed disk state before and after the trigger.

1. Invalid width `5` failed with `CALYX_STALE_DERIVED`; manifest SHA-256 stayed
   exactly `96dcd...b012`.
2. One flipped PQ byte changed its SHA-256 to `4d9468...945d`; query failed with
   the observed and manifest hashes before index open.
3. Renaming `graph.raw` made `exists=false`; query failed naming the exact path
   and OS error 2 before scoring.

The driver restored both files and independently read all three hashes again.
They exactly matched the happy-path values, and the known query again returned
the expected row first. No write return value is used as acceptance evidence.
