# Calyx-Dev post-absorption production-delta inventory

Tracking issue: [#1760](https://github.com/ChrisRoyse/Synapse/issues/1760)

## Scope and decision rule

- Absorption baseline: `9894f84f`
- Authenticated `ChrisRoyse/Calyx-Dev` `origin/main` head inspected: `313860bc1810267b0e84efc28c752988f308ea73`
- Range: `9894f84f..313860bc`, 283 commits, oldest-first
- Inspection date: 2026-07-19
- Source-of-truth clone: `C:\code\Calyx-Dev`
- Product source of truth: Synapse-owned `calyx/`; this is an audit ledger, never an instruction to merge or synchronize upstream history.

Each commit is represented exactly once below. Classification uses the commit's first-parent file delta so merge commits do not double-count their constituent changes. Runtime source/manifests under absorbed crates are conservatively **applicable** until a native semantic comparison proves them already adapted, superseded, or incompatible. Test, benchmark, FSV-driver, CI, docs, formatting, CLI, MCP/server, and other deliberately pruned surfaces are **not applicable**. This intentionally errs toward retaining production work in the queue.

An **applicable** row is inventory evidence, not acceptance evidence. It remains open work until its behavior/invariants have been compared, ported or rejected with a concrete rationale, compiled/linted, and manually FSV-proven through the real wired Synapse MCP with separate physical Source-of-Truth readback under D1.

## Coverage summary

| Classification | Commits |
| --- | ---: |
| applicable | 135 |
| already adapted | 2 |
| superseded | 0 |
| not applicable | 146 |
| **total** | **283** |

The zero `superseded` count is deliberate: no delta was labeled superseded merely because similar-looking code exists. That decision requires behavior/invariant evidence.

## Applicable runtime commits by absorbed crate

Counts overlap when one commit changes more than one absorbed crate.

| Crate | Applicable commits |
| --- | ---: |
| `sextant` | 66 |
| `search` | 30 |
| `forge` | 23 |
| `aster` | 10 |
| `lodestar` | 8 |
| `registry` | 8 |
| `assay` | 6 |
| `oracle` | 2 |
| `loom` | 1 |
| `mincut` | 1 |
| `ledger` | 1 |
| `core` | 1 |

## Commit ledger

| Commit | Date | Upstream subject | Classification | Evidence / next action |
| --- | --- | --- | --- | --- |
| `1945ee11` | 2026-07-15 | test(cli): drain resident service telemetry (#1788) | **not applicable** | Test/benchmark/documentation/formatting-only intent is excluded by the absorption and zero-test policies. |
| `7cc232be` | 2026-07-15 | fix(assay): accelerate dependence estimators on CUDA (#1797) | **applicable** | Runtime delta in absorbed `assay`, `forge`; native semantic comparison/port and manual FSV remain required. |
| `3247e1b6` | 2026-07-15 | feat(forge): batch unique-seed TurboQuant rows on CUDA | **applicable** | Runtime delta in absorbed `forge`; native semantic comparison/port and manual FSV remain required. |
| `0f7d3993` | 2026-07-15 | feat(aster): quantize streaming microbatches on CUDA | **applicable** | Runtime delta in absorbed `aster`; native semantic comparison/port and manual FSV remain required. |
| `d015747c` | 2026-07-15 | fix(aster): clean batched ingest borrow | **applicable** | Runtime delta in absorbed `aster`; native semantic comparison/port and manual FSV remain required. |
| `e72e8e85` | 2026-07-15 | feat(forge): add CUDA OLAP and tiled transpose | **applicable** | Runtime delta in absorbed `forge`; native semantic comparison/port and manual FSV remain required. |
| `2aab0e60` | 2026-07-15 | feat(aster): dispatch OLAP and transpose to CUDA | **applicable** | Runtime delta in absorbed `aster`; native semantic comparison/port and manual FSV remain required. |
| `a4469fc5` | 2026-07-15 | fix(forge): discard OLAP launch timing metadata | **applicable** | Runtime delta in absorbed `forge`; native semantic comparison/port and manual FSV remain required. |
| `9263aa3b` | 2026-07-15 | chore(forge): format merged CUDA exports | **applicable** | Runtime delta in absorbed `forge`; native semantic comparison/port and manual FSV remain required. |
| `be0d8083` | 2026-07-15 | chore(forge): format merged CUDA exports | **applicable** | Runtime delta in absorbed `forge`; native semantic comparison/port and manual FSV remain required. |
| `43e92659` | 2026-07-15 | feat(sextant): route large skill clustering to CUDA | **applicable** | Runtime delta in absorbed `forge`, `sextant`; native semantic comparison/port and manual FSV remain required. |
| `b75026cb` | 2026-07-15 | fix(forge): retain CUDA stream during skill launches | **applicable** | Runtime delta in absorbed `forge`; native semantic comparison/port and manual FSV remain required. |
| `d3c46a0e` | 2026-07-15 | fix(forge): use portable infinity in skill kernel | **applicable** | Runtime delta in absorbed `forge`; native semantic comparison/port and manual FSV remain required. |
| `89f18b9a` | 2026-07-15 | fix(sextant): type CUDA routing fixture dimensions | **not applicable** | No absorbed production path; touched `sextant`. |
| `8e87cf59` | 2026-07-15 | feat(forge): add resident CUDA energy descent | **applicable** | Runtime delta in absorbed `forge`; native semantic comparison/port and manual FSV remain required. |
| `32a93535` | 2026-07-15 | feat(oracle): dispatch large energy descent to CUDA | **applicable** | Runtime delta in absorbed `oracle`; native semantic comparison/port and manual FSV remain required. |
| `f54d1eca` | 2026-07-15 | fix(forge): use portable infinity in energy kernel | **applicable** | Runtime delta in absorbed `forge`; native semantic comparison/port and manual FSV remain required. |
| `894a6281` | 2026-07-15 | fix(forge): discard CUDA launch timing metadata | **applicable** | Runtime delta in absorbed `forge`; native semantic comparison/port and manual FSV remain required. |
| `67c1bc37` | 2026-07-15 | fix(forge): retain CUDA stream during energy launches | **applicable** | Runtime delta in absorbed `forge`; native semantic comparison/port and manual FSV remain required. |
| `8f7f40e0` | 2026-07-15 | test(oracle): benchmark CUDA complete against CPU | **not applicable** | Test/benchmark/documentation/formatting-only intent is excluded by the absorption and zero-test policies. |
| `b4e2780c` | 2026-07-15 | feat(oracle): export energy CUDA crossover | **applicable** | Runtime delta in absorbed `oracle`; native semantic comparison/port and manual FSV remain required. |
| `73ee81da` | 2026-07-15 | chore(forge): format merged CUDA exports | **applicable** | Runtime delta in absorbed `forge`; native semantic comparison/port and manual FSV remain required. |
| `9d352653` | 2026-07-15 | chore(forge): format merged CUDA exports | **applicable** | Runtime delta in absorbed `forge`; native semantic comparison/port and manual FSV remain required. |
| `2e42af13` | 2026-07-15 | feat(sextant): add strict GPU partition phases | **applicable** | Runtime delta in absorbed `sextant`; native semantic comparison/port and manual FSV remain required. |
| `ade1916f` | 2026-07-15 | fix(sextant): compile CUDA partition path | **applicable** | Runtime delta in absorbed `sextant`; native semantic comparison/port and manual FSV remain required. |
| `53b170ee` | 2026-07-15 | fix(sextant): release CUDA query guard before accounting | **applicable** | Runtime delta in absorbed `sextant`; native semantic comparison/port and manual FSV remain required. |
| `97f64c3d` | 2026-07-15 | fix(sextant): satisfy cuVS kmeans tolerance contract | **applicable** | Runtime delta in absorbed `sextant`; native semantic comparison/port and manual FSV remain required. |
| `61c37e8c` | 2026-07-15 | fix(sextant): use stable cuVS routing id ABI | **applicable** | Runtime delta in absorbed `sextant`; native semantic comparison/port and manual FSV remain required. |
| `fc7d9afb` | 2026-07-15 | fix(sextant): widen CUDA routing buffer contract | **applicable** | Runtime delta in absorbed `sextant`; native semantic comparison/port and manual FSV remain required. |
| `df1abb33` | 2026-07-15 | test(sextant): cover strict CUDA partition phases | **not applicable** | Test/benchmark/documentation/formatting-only intent is excluded by the absorption and zero-test policies. |
| `b189b49d` | 2026-07-15 | perf(sextant): plan GPU partitions toward balance cap | **applicable** | Runtime delta in absorbed `sextant`; native semantic comparison/port and manual FSV remain required. |
| `d6c8c730` | 2026-07-15 | perf(sextant): batch strict GPU partition balance | **applicable** | Runtime delta in absorbed `sextant`; native semantic comparison/port and manual FSV remain required. |
| `82737827` | 2026-07-15 | perf(sextant): amortize streaming partition scans | **applicable** | Runtime delta in absorbed `sextant`; native semantic comparison/port and manual FSV remain required. |
| `d6d6570a` | 2026-07-15 | fix(sextant): bound GPU k-means workspace | **applicable** | Runtime delta in absorbed `sextant`; native semantic comparison/port and manual FSV remain required. |
| `92e09cfe` | 2026-07-15 | fix(sextant): stream GPU k-means samples | **applicable** | Runtime delta in absorbed `sextant`; native semantic comparison/port and manual FSV remain required. |
| `2f045e54` | 2026-07-15 | fix(sextant): scale strict GPU balance depth | **applicable** | Runtime delta in absorbed `sextant`; native semantic comparison/port and manual FSV remain required. |
| `183e86f4` | 2026-07-15 | fix(sextant): allow bounded GPU balance rounds | **applicable** | Runtime delta in absorbed `sextant`; native semantic comparison/port and manual FSV remain required. |
| `f0258989` | 2026-07-15 | fix(sextant): terminate strict GPU cap balancing | **applicable** | Runtime delta in absorbed `sextant`; native semantic comparison/port and manual FSV remain required. |
| `9835f182` | 2026-07-15 | perf(sextant): reuse cap-headroom GPU centroids | **applicable** | Runtime delta in absorbed `sextant`; native semantic comparison/port and manual FSV remain required. |
| `4b75bebe` | 2026-07-15 | perf(sextant): skip redundant GPU provisional scan | **applicable** | Runtime delta in absorbed `sextant`; native semantic comparison/port and manual FSV remain required. |
| `125b1673` | 2026-07-15 | fix(sextant): extend GPU assignment capacity probe | **applicable** | Runtime delta in absorbed `sextant`; native semantic comparison/port and manual FSV remain required. |
| `20cae864` | 2026-07-15 | fix(sextant): preserve GPU closure radius under cap pressure | **applicable** | Runtime delta in absorbed `sextant`; native semantic comparison/port and manual FSV remain required. |
| `d0796672` | 2026-07-15 | fix(sextant): balance GPU partition centroids | **applicable** | Runtime delta in absorbed `sextant`; native semantic comparison/port and manual FSV remain required. |
| `ace9b30e` | 2026-07-15 | test(sextant): clean GPU replay imports | **not applicable** | Test/benchmark/documentation/formatting-only intent is excluded by the absorption and zero-test policies. |
| `0aa033b8` | 2026-07-15 | test(sextant): parameterize GPU closure replay | **not applicable** | Test/benchmark/documentation/formatting-only intent is excluded by the absorption and zero-test policies. |
| `ed689747` | 2026-07-15 | fix(sextant): calibrate one-pass GPU closure support | **applicable** | Runtime delta in absorbed `sextant`; native semantic comparison/port and manual FSV remain required. |
| `a62f2992` | 2026-07-15 | fix(fsv): read partition manifests from graph CF | **not applicable** | No absorbed production path; touched `scripts`. |
| `108d0144` | 2026-07-15 | fix(sextant): calibrate sparse partition closure | **applicable** | Runtime delta in absorbed `sextant`; native semantic comparison/port and manual FSV remain required. |
| `421fb3d9` | 2026-07-15 | fix(sextant): retain one-pass partition headroom | **applicable** | Runtime delta in absorbed `sextant`; native semantic comparison/port and manual FSV remain required. |
| `3b3a1092` | 2026-07-15 | refactor(sextant): isolate partition build validation | **applicable** | Runtime delta in absorbed `sextant`; native semantic comparison/port and manual FSV remain required. |
| `c559669a` | 2026-07-15 | fix(sextant): calibrate one-pass replica closure | **applicable** | Runtime delta in absorbed `sextant`; native semantic comparison/port and manual FSV remain required. |
| `363a00b5` | 2026-07-15 | test(oracle): benchmark warmed contract energy shape | **not applicable** | Test/benchmark/documentation/formatting-only intent is excluded by the absorption and zero-test policies. |
| `ef0b886e` | 2026-07-15 | Route Loom production weaving through CUDA batches (#1804) | **applicable** | Runtime delta in absorbed `assay`, `forge`, `loom`; native semantic comparison/port and manual FSV remain required. |
| `4a54d03b` | 2026-07-15 | test(olap): report issue 1519 phase timings | **not applicable** | Test/benchmark/documentation/formatting-only intent is excluded by the absorption and zero-test policies. |
| `4b2a57f3` | 2026-07-15 | perf(forge): reuse bounded OLAP transpose buffers | **applicable** | Runtime delta in absorbed `forge`; native semantic comparison/port and manual FSV remain required. |
| `77b0f2c2` | 2026-07-15 | perf(forge): read transpose output through cacheable memory | **applicable** | Runtime delta in absorbed `forge`; native semantic comparison/port and manual FSV remain required. |
| `4b53b226` | 2026-07-15 | perf(aster): append CUDA column payload in one pass | **applicable** | Runtime delta in absorbed `aster`; native semantic comparison/port and manual FSV remain required. |
| `0ba5cb6c` | 2026-07-15 | perf(forge): read transpose columns into final output | **applicable** | Runtime delta in absorbed `forge`; native semantic comparison/port and manual FSV remain required. |
| `e394003c` | 2026-07-15 | perf(forge): parallelize OLAP pinned staging | **applicable** | Runtime delta in absorbed `forge`; native semantic comparison/port and manual FSV remain required. |
| `89b88f42` | 2026-07-15 | test(aster): benchmark OLAP medians with parity samples | **not applicable** | Test/benchmark/documentation/formatting-only intent is excluded by the absorption and zero-test policies. |
| `105d04ce` | 2026-07-15 | Merge origin/main into fix/issue1517-gpu-skills | **not applicable** | Merge integration only; constituent commits are classified separately in this inventory. |
| `b142b727` | 2026-07-15 | Merge pull request #1800 from ChrisRoyse/fix/issue1517-gpu-skills | **not applicable** | Merge integration only; constituent commits are classified separately in this inventory. |
| `a98f8d2c` | 2026-07-15 | Merge origin/main into fix/issue1521-gpu-energy | **not applicable** | Merge integration only; constituent commits are classified separately in this inventory. |
| `9656cca6` | 2026-07-15 | Merge pull request #1801 from ChrisRoyse/fix/issue1521-gpu-energy | **not applicable** | Merge integration only; constituent commits are classified separately in this inventory. |
| `355dee2f` | 2026-07-15 | Merge origin/main into fix/issue1518-gpu-stream | **not applicable** | Merge integration only; constituent commits are classified separately in this inventory. |
| `7c840676` | 2026-07-15 | Merge pull request #1803 from ChrisRoyse/fix/issue1518-gpu-stream | **not applicable** | Merge integration only; constituent commits are classified separately in this inventory. |
| `5b63cd8a` | 2026-07-15 | Merge origin/main into fix/issue1519-gpu-olap | **not applicable** | Merge integration only; constituent commits are classified separately in this inventory. |
| `15eb1214` | 2026-07-15 | Merge pull request #1806 from ChrisRoyse/fix/issue1519-gpu-olap | **not applicable** | Merge integration only; constituent commits are classified separately in this inventory. |
| `166804dc` | 2026-07-15 | feat(search): add CUDA dense serving | **applicable** | Runtime delta in absorbed `search`, `sextant`; native semantic comparison/port and manual FSV remain required. |
| `7e4c9d2a` | 2026-07-15 | test(search): cover CUDA dense serving parity | **not applicable** | Test/benchmark/documentation/formatting-only intent is excluded by the absorption and zero-test policies. |
| `9d65c5ee` | 2026-07-15 | fix(search): keep CAGRA filters in device kernel | **applicable** | Runtime delta in absorbed `sextant`; native semantic comparison/port and manual FSV remain required. |
| `4c0094a7` | 2026-07-15 | test(search): remove CAGRA crash probes | **not applicable** | Test/benchmark/documentation/formatting-only intent is excluded by the absorption and zero-test policies. |
| `dde51c4e` | 2026-07-15 | fix(search): reconcile bounded device caches | **applicable** | Runtime delta in absorbed `search`, `sextant`; native semantic comparison/port and manual FSV remain required. |
| `4e87a821` | 2026-07-15 | fix(search): honor strided CAGRA datasets | **applicable** | Runtime delta in absorbed `sextant`; native semantic comparison/port and manual FSV remain required. |
| `6ee17120` | 2026-07-15 | fix(search): create CAGRA asset directories | **applicable** | Runtime delta in absorbed `sextant`; native semantic comparison/port and manual FSV remain required. |
| `fcbeb1f5` | 2026-07-15 | perf(search): batch partitioned CUDA serving | **applicable** | Runtime delta in absorbed `sextant`; native semantic comparison/port and manual FSV remain required. |
| `ce81daca` | 2026-07-15 | test(search): cover partitioned CUDA batch merge | **not applicable** | Test/benchmark/documentation/formatting-only intent is excluded by the absorption and zero-test policies. |
| `9826a21b` | 2026-07-15 | perf(search): parallelize partitioned CUDA scans | **applicable** | Runtime delta in absorbed `sextant`; native semantic comparison/port and manual FSV remain required. |
| `2fe2b127` | 2026-07-15 | fix(search): match partitioned CUDA launch geometry | **applicable** | Runtime delta in absorbed `sextant`; native semantic comparison/port and manual FSV remain required. |
| `ee6960dc` | 2026-07-15 | perf(search): snapshot partitioned CUDA assets | **applicable** | Runtime delta in absorbed `sextant`; native semantic comparison/port and manual FSV remain required. |
| `7664f14a` | 2026-07-15 | perf(search): expose bounded CUDA batch timing | **applicable** | Runtime delta in absorbed `sextant`; native semantic comparison/port and manual FSV remain required. |
| `548eae79` | 2026-07-15 | perf(search): persist compact CUDA region datasets | **applicable** | Runtime delta in absorbed `sextant`; native semantic comparison/port and manual FSV remain required. |
| `8a97d09a` | 2026-07-15 | fix(search): import compact dataset build metric | **applicable** | Runtime delta in absorbed `sextant`; native semantic comparison/port and manual FSV remain required. |
| `25cdad31` | 2026-07-15 | perf(search): share compact dataset CUDA stream | **applicable** | Runtime delta in absorbed `sextant`; native semantic comparison/port and manual FSV remain required. |
| `67434bad` | 2026-07-15 | fix(search): bind compact cache to payload identity | **applicable** | Runtime delta in absorbed `sextant`; native semantic comparison/port and manual FSV remain required. |
| `e91e71c9` | 2026-07-15 | perf(search): index CUDA cache generations | **applicable** | Runtime delta in absorbed `sextant`; native semantic comparison/port and manual FSV remain required. |
| `3bf268c4` | 2026-07-15 | perf(search): bound CUDA cache eviction lookup | **applicable** | Runtime delta in absorbed `sextant`; native semantic comparison/port and manual FSV remain required. |
| `b0246dae` | 2026-07-15 | perf(search): coalesce compact asset uploads | **applicable** | Runtime delta in absorbed `sextant`; native semantic comparison/port and manual FSV remain required. |
| `bb3b60bf` | 2026-07-15 | test(search): replace compact CUDA generations | **not applicable** | Test/benchmark/documentation/formatting-only intent is excluded by the absorption and zero-test policies. |
| `ee4f74c6` | 2026-07-15 | fix(search): bind compact cache to id maps | **applicable** | Runtime delta in absorbed `sextant`; native semantic comparison/port and manual FSV remain required. |
| `45b889a6` | 2026-07-15 | style(search): format CUDA serving changes | **not applicable** | Test/benchmark/documentation/formatting-only intent is excluded by the absorption and zero-test policies. |
| `9cbc2731` | 2026-07-15 | fix(search): key compact id generations | **applicable** | Runtime delta in absorbed `sextant`; native semantic comparison/port and manual FSV remain required. |
| `90a897fc` | 2026-07-15 | fix(sextant): balance GPU primary regions | **applicable** | Runtime delta in absorbed `sextant`; native semantic comparison/port and manual FSV remain required. |
| `8c89525a` | 2026-07-15 | test(sextant): expect balanced CUDA backend | **not applicable** | Test/benchmark/documentation/formatting-only intent is excluded by the absorption and zero-test policies. |
| `fac59023` | 2026-07-15 | test(sextant): expect one-pass CUDA transfers | **not applicable** | Test/benchmark/documentation/formatting-only intent is excluded by the absorption and zero-test policies. |
| `bc103a50` | 2026-07-15 | Merge remote-tracking branch 'origin/main' into fix/issue1515-gpu-partition | **not applicable** | Merge integration only; constituent commits are classified separately in this inventory. |
| `f5f4686b` | 2026-07-15 | fix(sextant): calibrate balanced GPU closure | **applicable** | Runtime delta in absorbed `sextant`; native semantic comparison/port and manual FSV remain required. |
| `1b3c9985` | 2026-07-15 | fix(search): reclaim evicted CUDA pool pages | **applicable** | Runtime delta in absorbed `sextant`; native semantic comparison/port and manual FSV remain required. |
| `d052897d` | 2026-07-15 | fix(search): bound CUDA pool across invalidations | **applicable** | Runtime delta in absorbed `sextant`; native semantic comparison/port and manual FSV remain required. |
| `340782b3` | 2026-07-15 | Merge pull request #1811 from ChrisRoyse/fix/issue1515-gpu-partition | **not applicable** | Merge integration only; constituent commits are classified separately in this inventory. |
| `6b033f0e` | 2026-07-15 | style(search): format CUDA pool helpers | **not applicable** | Test/benchmark/documentation/formatting-only intent is excluded by the absorption and zero-test policies. |
| `0cad9572` | 2026-07-15 | Fix issue1510 clippy gate warnings | **applicable** | Runtime delta in absorbed `sextant`; native semantic comparison/port and manual FSV remain required. |
| `9fa295e9` | 2026-07-15 | Fix issue1510 search clippy gate warning | **applicable** | Runtime delta in absorbed `search`; native semantic comparison/port and manual FSV remain required. |
| `0f090610` | 2026-07-15 | Refresh fuzz lockfile for gate | **not applicable** | No absorbed production path; touched `fuzz`. |
| `5f0c9191` | 2026-07-15 | Pin fuzz lockfile to vendored libfuzzer | **not applicable** | No absorbed production path; touched `fuzz`. |
| `98952436` | 2026-07-15 | Align fuzz lockfile with vendored gate | **not applicable** | No absorbed production path; touched `fuzz`. |
| `33de067d` | 2026-07-15 | Fix orphan Rust gate for CUDA sources | **not applicable** | No absorbed production path; touched `scripts`. |
| `ac0dffaa` | 2026-07-15 | Implement sparse BM25 CUDA top-k | **applicable** | Runtime delta in absorbed `search`, `sextant`; native semantic comparison/port and manual FSV remain required. |
| `522d62c9` | 2026-07-15 | Add chunked CUDA MaxSim serving | **applicable** | Runtime delta in absorbed `search`, `sextant`; native semantic comparison/port and manual FSV remain required. |
| `4b5fdde1` | 2026-07-15 | Merge remote-tracking branch 'origin/main' into fix/law-poc-cuyahoga-canonical | **not applicable** | Merge integration only; constituent commits are classified separately in this inventory. |
| `06bc8546` | 2026-07-15 | fix(mcp): retain validated search panel runtime (#1815) | **not applicable** | No absorbed production path; touched `mcp`. |
| `be770323` | 2026-07-15 | docs(law): record MCP runtime cache FSV (#1815) | **not applicable** | Test/benchmark/documentation/formatting-only intent is excluded by the absorption and zero-test policies. |
| `21c34a01` | 2026-07-15 | fix(search): overlap frozen query lenses (#1816) | **applicable** | Runtime delta in absorbed `search`; native semantic comparison/port and manual FSV remain required. |
| `d80724e8` | 2026-07-15 | fix(mcp): verify cached panel assets in place (#1817) | **not applicable** | No absorbed production path; touched `mcp`. |
| `12bbf9f7` | 2026-07-15 | Cache opened MCP search vault for #1817 | **not applicable** | No absorbed production path; touched `mcp`. |
| `2685844e` | 2026-07-15 | Honor migrated derived frontier in #1817 cache | **not applicable** | No absorbed production path; touched `mcp`. |
| `15a1cb90` | 2026-07-15 | Document #1817 production cache FSV | **not applicable** | No absorbed production path; touched `docs`. |
| `f318ded4` | 2026-07-15 | Correct #1817 evidence hash labels | **not applicable** | No absorbed production path; touched `docs`. |
| `411c5c12` | 2026-07-15 | Document #1816 production measurement FSV | **not applicable** | No absorbed production path; touched `docs`. |
| `d106bf14` | 2026-07-15 | Accelerate canonical MaxSim search for #1813 | **applicable** | Runtime delta in absorbed `search`; native semantic comparison/port and manual FSV remain required. |
| `fab0c1bd` | 2026-07-15 | Document #1813 production search FSV | **not applicable** | No absorbed production path; touched `docs`. |
| `13423d27` | 2026-07-15 | Merge issue1510 dense serving CUDA | **not applicable** | Merge integration only; constituent commits are classified separately in this inventory. |
| `dbe53eb2` | 2026-07-15 | Merge issue1511 MaxSim CUDA | **not applicable** | Merge integration only; constituent commits are classified separately in this inventory. |
| `52fae2e5` | 2026-07-15 | Merge issue1512 sparse BM25 CUDA | **not applicable** | Merge integration only; constituent commits are classified separately in this inventory. |
| `f1895622` | 2026-07-15 | Reuse stable vault snapshots for search provenance | **applicable** | Runtime delta in absorbed `search`; native semantic comparison/port and manual FSV remain required. |
| `920cd3ad` | 2026-07-15 | Split merged CUDA modules under line limit | **applicable** | Runtime delta in absorbed `search`, `sextant`; native semantic comparison/port and manual FSV remain required. |
| `cc42ebac` | 2026-07-15 | Preserve physical ledger verification in cached view for #1818 | **applicable** | Runtime delta in absorbed `aster`, `search`; native semantic comparison/port and manual FSV remain required. |
| `7fe0b6f2` | 2026-07-16 | Document physical ledger view FSV for #1818 | **not applicable** | No absorbed production path; touched `docs`. |
| `87286ff4` | 2026-07-16 | Bound candidate MaxSim CUDA serving for #1820 | **applicable** | Runtime delta in absorbed `search`; native semantic comparison/port and manual FSV remain required. |
| `b3421e15` | 2026-07-16 | Repair CUDA lint gate for #1822 | **applicable** | Runtime delta in absorbed `sextant`; native semantic comparison/port and manual FSV remain required. |
| `2a63b0ef` | 2026-07-16 | Fix CUDA telemetry lint for #1820 | **applicable** | Runtime delta in absorbed `search`; native semantic comparison/port and manual FSV remain required. |
| `4927c57f` | 2026-07-16 | Correct MaxSim telemetry visibility for #1820 | **applicable** | Runtime delta in absorbed `search`; native semantic comparison/port and manual FSV remain required. |
| `755aa6e7` | 2026-07-16 | perf(search): pin candidate MaxSim CUDA staging | **applicable** | Runtime delta in absorbed `search`, `sextant`; native semantic comparison/port and manual FSV remain required. |
| `9aca64df` | 2026-07-16 | fix(search): normalize async CUDA launch results | **applicable** | Runtime delta in absorbed `sextant`; native semantic comparison/port and manual FSV remain required. |
| `2b93194f` | 2026-07-16 | fix(search): satisfy CUDA release lint | **applicable** | Runtime delta in absorbed `sextant`; native semantic comparison/port and manual FSV remain required. |
| `7918aafe` | 2026-07-16 | fix(search): export CUDA host staging buffer | **applicable** | Runtime delta in absorbed `sextant`; native semantic comparison/port and manual FSV remain required. |
| `5693a30c` | 2026-07-16 | fix(search): remove obsolete CUDA chunk accessors | **applicable** | Runtime delta in absorbed `search`; native semantic comparison/port and manual FSV remain required. |
| `b2c01cb1` | 2026-07-16 | perf(search): parallelize resident CUDA stage one | **applicable** | Runtime delta in absorbed `search`, `sextant`; native semantic comparison/port and manual FSV remain required. |
| `e79fa35f` | 2026-07-16 | fix(search): satisfy CUDA resident lint | **applicable** | Runtime delta in absorbed `sextant`; native semantic comparison/port and manual FSV remain required. |
| `7c67bf83` | 2026-07-16 | perf(search): score admitted CUDA rows only | **applicable** | Runtime delta in absorbed `sextant`; native semantic comparison/port and manual FSV remain required. |
| `78b8b85c` | 2026-07-16 | Revert "perf(search): score admitted CUDA rows only" | **applicable** | Runtime delta in absorbed `sextant`; native semantic comparison/port and manual FSV remain required. |
| `1a83e7bd` | 2026-07-16 | perf(search): route small persisted slots explicitly | **applicable** | Runtime delta in absorbed `search`; native semantic comparison/port and manual FSV remain required. |
| `d7c4821b` | 2026-07-16 | fix(search): evict changed MaxSim generations | **applicable** | Runtime delta in absorbed `search`; native semantic comparison/port and manual FSV remain required. |
| `ec36b8ce` | 2026-07-16 | fix(search): invalidate MaxSim during bounded preflight | **applicable** | Runtime delta in absorbed `search`; native semantic comparison/port and manual FSV remain required. |
| `f1b2662c` | 2026-07-16 | docs(law): close MaxSim warm-path regression #1820 | **not applicable** | Test/benchmark/documentation/formatting-only intent is excluded by the absorption and zero-test policies. |
| `39c95a0f` | 2026-07-16 | docs(law): close exact CUDA lint #1822 | **not applicable** | Test/benchmark/documentation/formatting-only intent is excluded by the absorption and zero-test policies. |
| `13022159` | 2026-07-16 | docs(law): close MaxSim module split #1821 | **not applicable** | Test/benchmark/documentation/formatting-only intent is excluded by the absorption and zero-test policies. |
| `49080de5` | 2026-07-16 | Add explicit law search reranking | **applicable** | Runtime delta in absorbed `search`, `sextant`; native semantic comparison/port and manual FSV remain required. |
| `d6fcd9ae` | 2026-07-16 | docs(law): close canonical Cuyahoga extraction #1463 | **not applicable** | Test/benchmark/documentation/formatting-only intent is excluded by the absorption and zero-test policies. |
| `5c5a8b96` | 2026-07-16 | docs(law): close canonical ingest aliases #1464 | **not applicable** | Test/benchmark/documentation/formatting-only intent is excluded by the absorption and zero-test policies. |
| `1998c94f` | 2026-07-16 | docs(law): close corrected judge mapping #1466 | **not applicable** | Test/benchmark/documentation/formatting-only intent is excluded by the absorption and zero-test policies. |
| `366c5d51` | 2026-07-16 | docs(law): close corrected citation graph #1465 | **not applicable** | Test/benchmark/documentation/formatting-only intent is excluded by the absorption and zero-test policies. |
| `3b8ba000` | 2026-07-16 | Add resident CLI reranking with recall headroom | **applicable** | Runtime delta in absorbed `search`; native semantic comparison/port and manual FSV remain required. |
| `24acd496` | 2026-07-16 | Wire search CUDA into unified GPU builds | **applicable** | Runtime delta in absorbed `search`; native semantic comparison/port and manual FSV remain required. |
| `2fcc99e6` | 2026-07-16 | Preserve lexical specificity in semantic reranking | **applicable** | Runtime delta in absorbed `search`, `sextant`; native semantic comparison/port and manual FSV remain required. |
| `1f7ed617` | 2026-07-16 | Stabilize public CUDA reranker scores | **applicable** | Runtime delta in absorbed `search`; native semantic comparison/port and manual FSV remain required. |
| `29d32ada` | 2026-07-16 | docs(law): close search CUDA feature #1835 | **not applicable** | Test/benchmark/documentation/formatting-only intent is excluded by the absorption and zero-test policies. |
| `2dccad5d` | 2026-07-16 | Preserve coherent phrases in semantic reranking | **applicable** | Runtime delta in absorbed `search`, `sextant`; native semantic comparison/port and manual FSV remain required. |
| `6d569b8a` | 2026-07-16 | docs(law): close canonical pilot gate #1468 | **not applicable** | Test/benchmark/documentation/formatting-only intent is excluded by the absorption and zero-test policies. |
| `94c1523f` | 2026-07-16 | Speed exact metadata reranking | **applicable** | Runtime delta in absorbed `search`, `sextant`; native semantic comparison/port and manual FSV remain required. |
| `8cc60356` | 2026-07-16 | Bound exact metadata retrieval work | **applicable** | Runtime delta in absorbed `search`; native semantic comparison/port and manual FSV remain required. |
| `18e9e8dd` | 2026-07-16 | docs(law): close exact search defect #1827 | **not applicable** | Test/benchmark/documentation/formatting-only intent is excluded by the absorption and zero-test policies. |
| `3e01ce6a` | 2026-07-16 | docs(law): close search SLO defect #1829 | **not applicable** | Test/benchmark/documentation/formatting-only intent is excluded by the absorption and zero-test policies. |
| `b7bd27d2` | 2026-07-16 | docs(law): close semantic search defect #1828 | **not applicable** | Test/benchmark/documentation/formatting-only intent is excluded by the absorption and zero-test policies. |
| `f06051a9` | 2026-07-16 | docs(law): close production search gate #1470 | **not applicable** | Test/benchmark/documentation/formatting-only intent is excluded by the absorption and zero-test policies. |
| `23a3a057` | 2026-07-16 | docs(law): close scoped ZFS blocker #1775 | **not applicable** | Test/benchmark/documentation/formatting-only intent is excluded by the absorption and zero-test policies. |
| `b3187f13` | 2026-07-16 | docs(law): close bounded rebuild defect #1799 | **not applicable** | Test/benchmark/documentation/formatting-only intent is excluded by the absorption and zero-test policies. |
| `f55c8f98` | 2026-07-16 | docs(law): close canonical full ingest #1469 | **not applicable** | Test/benchmark/documentation/formatting-only intent is excluded by the absorption and zero-test policies. |
| `bfe31521` | 2026-07-16 | fix(weave): persist strict association weight (#1837) | **not applicable** | No absorbed production path; touched `cli`. |
| `1a665caa` | 2026-07-16 | docs(weave): close strict edge weight defect #1837 | **not applicable** | Test/benchmark/documentation/formatting-only intent is excluded by the absorption and zero-test policies. |
| `1b42f984` | 2026-07-16 | docs(law): close full association weave #1471 | **not applicable** | Test/benchmark/documentation/formatting-only intent is excluded by the absorption and zero-test policies. |
| `7406a8d2` | 2026-07-16 | docs(law): close typed citation overlay #1472 | **not applicable** | Test/benchmark/documentation/formatting-only intent is excluded by the absorption and zero-test policies. |
| `1d39bd67` | 2026-07-16 | fix(kernel): redact retained admission path (#1839) | **not applicable** | No absorbed production path; touched `cli`. |
| `b875f82f` | 2026-07-16 | fix(weave): verify durable CSR publication (#1838) | **not applicable** | No absorbed production path; touched `cli`. |
| `785d3368` | 2026-07-16 | fix(kernel): name retained query digest explicitly (#1839) | **not applicable** | No absorbed production path; touched `cli`. |
| `1b5f0f65` | 2026-07-16 | docs(kernel): close ledger payload defect #1839 | **not applicable** | Test/benchmark/documentation/formatting-only intent is excluded by the absorption and zero-test policies. |
| `e84c509a` | 2026-07-16 | docs(law): close grounding kernel #1473 | **not applicable** | Test/benchmark/documentation/formatting-only intent is excluded by the absorption and zero-test policies. |
| `ee9e6ca5` | 2026-07-16 | docs(weave): close CSR durability defect #1838 | **not applicable** | Test/benchmark/documentation/formatting-only intent is excluded by the absorption and zero-test policies. |
| `1ff35568` | 2026-07-16 | docs(law): close categorical assay #1474 | **not applicable** | Test/benchmark/documentation/formatting-only intent is excluded by the absorption and zero-test policies. |
| `27f7c730` | 2026-07-16 | fix(lodestar): normalize multiway spectral embedding (#1749) | **applicable** | Runtime delta in absorbed `lodestar`, `mincut`; native semantic comparison/port and manual FSV remain required. |
| `6f8b7348` | 2026-07-16 | docs(lodestar): close normalized communities defect #1749 | **not applicable** | Test/benchmark/documentation/formatting-only intent is excluded by the absorption and zero-test policies. |
| `b3abfab6` | 2026-07-16 | docs(law): close communities probe #1475 | **not applicable** | Test/benchmark/documentation/formatting-only intent is excluded by the absorption and zero-test policies. |
| `93af4270` | 2026-07-16 | docs(law): close doctrine-kernel demo #1476 | **not applicable** | Test/benchmark/documentation/formatting-only intent is excluded by the absorption and zero-test policies. |
| `ebc7e31a` | 2026-07-16 | docs(law): close judge intelligence demo #1477 | **not applicable** | Test/benchmark/documentation/formatting-only intent is excluded by the absorption and zero-test policies. |
| `ffd5af08` | 2026-07-16 | fix(law): make cite-check production-safe (#1850) | **not applicable** | No absorbed production path; touched `docs`, `tools`. |
| `2cbf6baf` | 2026-07-16 | docs(law): close target-conditioned cite support #1851 | **not applicable** | Test/benchmark/documentation/formatting-only intent is excluded by the absorption and zero-test policies. |
| `aefbd247` | 2026-07-16 | docs(law): close sanctions-shield demo #1478 | **not applicable** | Test/benchmark/documentation/formatting-only intent is excluded by the absorption and zero-test policies. |
| `0d9cb08e` | 2026-07-16 | docs(law): close dissent-intelligence demo #1479 | **not applicable** | Test/benchmark/documentation/formatting-only intent is excluded by the absorption and zero-test policies. |
| `e256e126` | 2026-07-16 | docs(law): point graph readback gap to #1836 | **not applicable** | Test/benchmark/documentation/formatting-only intent is excluded by the absorption and zero-test policies. |
| `5f3913b2` | 2026-07-16 | fix(law): make kernel Answer ledger-safe (#1855) | **applicable** | Runtime delta in absorbed `lodestar`; native semantic comparison/port and manual FSV remain required. |
| `01b7cab2` | 2026-07-16 | docs(law): close grounded Q&A demo (#1480) | **not applicable** | Test/benchmark/documentation/formatting-only intent is excluded by the absorption and zero-test policies. |
| `f441e730` | 2026-07-16 | feat(law): add one-command demo harness (#1481) | **not applicable** | No absorbed production path; touched `docs`, `tools`. |
| `0520fab5` | 2026-07-16 | docs(law): roll up Cuyahoga PoC evidence (#1482) | **not applicable** | Test/benchmark/documentation/formatting-only intent is excluded by the absorption and zero-test policies. |
| `0f9b7932` | 2026-07-16 | docs(law): record PoC closeout lifecycle (#1482) | **not applicable** | Test/benchmark/documentation/formatting-only intent is excluded by the absorption and zero-test policies. |
| `f0c4e544` | 2026-07-16 | docs(law): seal Cuyahoga PoC epic (#1460) | **not applicable** | Test/benchmark/documentation/formatting-only intent is excluded by the absorption and zero-test policies. |
| `08603590` | 2026-07-17 | fix: preserve constellation panels in kernels (#1857) | **applicable** | Runtime delta in absorbed `lodestar`; native semantic comparison/port and manual FSV remain required. |
| `fb6cbf18` | 2026-07-17 | fix: publish kernel answer ledgers atomically (#1856) | **applicable** | Runtime delta in absorbed `aster`, `ledger`, `lodestar`; native semantic comparison/port and manual FSV remain required. |
| `f4fa3164` | 2026-07-17 | fix: ground kernel answers in retained sources (#1859) | **applicable** | Runtime delta in absorbed `aster`, `lodestar`; native semantic comparison/port and manual FSV remain required. |
| `0d382d80` | 2026-07-17 | feat: add typed citation kernel answers (#1858) | **applicable** | Runtime delta in absorbed `lodestar`; native semantic comparison/port and manual FSV remain required. |
| `4de5b087` | 2026-07-17 | fix(law): lazy-load authoritative PDF extractor (#1849) | **not applicable** | No absorbed production path; touched `docs`, `tools`. |
| `2b0464d2` | 2026-07-17 | feat(law): audit canonical production provenance (#1603) | **not applicable** | No absorbed production path; touched `docs`, `tools`. |
| `d0173585` | 2026-07-17 | feat(law): audit source-bound docket corrections (#1635) | **not applicable** | No absorbed production path; touched `docs`, `tools`. |
| `56fd7e8b` | 2026-07-17 | docs(law): verify authoritative PDF supplements (#1658) | **not applicable** | Test/benchmark/documentation/formatting-only intent is excluded by the absorption and zero-test policies. |
| `e2038238` | 2026-07-17 | docs(law): verify authoritative text selection (#1632) | **not applicable** | Test/benchmark/documentation/formatting-only intent is excluded by the absorption and zero-test policies. |
| `ed1fef60` | 2026-07-17 | fix(law): preserve source lineage end to end (#1630) | **not applicable** | No absorbed production path; touched `docs`, `tools`. |
| `183a9f9c` | 2026-07-17 | fix(law): preserve opinion aliases and citation provenance (#1629) | **not applicable** | No absorbed production path; touched `cli`, `docs`, `tools`. |
| `047e72ae` | 2026-07-17 | docs(law): close one-pass spool durability proof (#1726) | **not applicable** | Test/benchmark/documentation/formatting-only intent is excluded by the absorption and zero-test policies. |
| `d038727f` | 2026-07-17 | docs(law): close resolved spool replay proof (#1758) | **not applicable** | Test/benchmark/documentation/formatting-only intent is excluded by the absorption and zero-test policies. |
| `3525bcb9` | 2026-07-17 | docs(law): require resident-first panel swaps (#1834) | **not applicable** | Test/benchmark/documentation/formatting-only intent is excluded by the absorption and zero-test policies. |
| `75af6a4c` | 2026-07-17 | docs(law): make manifest watchers one-shot (#1785) | **not applicable** | Test/benchmark/documentation/formatting-only intent is excluded by the absorption and zero-test policies. |
| `3421655c` | 2026-07-17 | docs(law): prove durable extractor supervision (#1763) | **not applicable** | Test/benchmark/documentation/formatting-only intent is excluded by the absorption and zero-test policies. |
| `7dbfbc14` | 2026-07-17 | docs(law): bind vault to canonical ingest generation (#1833) | **not applicable** | Test/benchmark/documentation/formatting-only intent is excluded by the absorption and zero-test policies. |
| `e89efd94` | 2026-07-17 | docs(law): prove crash-durable extraction generation (#1634) | **not applicable** | Test/benchmark/documentation/formatting-only intent is excluded by the absorption and zero-test policies. |
| `3daccebb` | 2026-07-17 | docs(law): prove downstream generation durability (#1639) | **not applicable** | Test/benchmark/documentation/formatting-only intent is excluded by the absorption and zero-test policies. |
| `53834412` | 2026-07-17 | docs(law): prove canonical audit publication (#1640) | **not applicable** | Test/benchmark/documentation/formatting-only intent is excluded by the absorption and zero-test policies. |
| `235fc57a` | 2026-07-17 | docs(law): prove invalid-tenure quarantine (#1693) | **not applicable** | Test/benchmark/documentation/formatting-only intent is excluded by the absorption and zero-test policies. |
| `f619cb9a` | 2026-07-17 | fix(law): ground Kilbane author correction (#1848) | **not applicable** | No absorbed production path; touched `cli`, `docs`, `tools`. |
| `9fce83d5` | 2026-07-17 | docs(law): publish DB-native judge authority (#1854) | **not applicable** | Test/benchmark/documentation/formatting-only intent is excluded by the absorption and zero-test policies. |
| `090d0151` | 2026-07-17 | docs(law): close judge resolution follow-ups | **not applicable** | Test/benchmark/documentation/formatting-only intent is excluded by the absorption and zero-test policies. |
| `4f1af10b` | 2026-07-17 | fix(law): require structured remediation envelopes | **not applicable** | No absorbed production path; touched `docs`, `tools`. |
| `7df5ff56` | 2026-07-17 | fix(law): account every candidate cluster partition | **not applicable** | No absorbed production path; touched `docs`, `tools`. |
| `e646f612` | 2026-07-18 | perf(aster): port Poly vault engine improvements | **applicable** | Runtime delta in absorbed `aster`; native semantic comparison/port and manual FSV remain required. |
| `18bd8e18` | 2026-07-18 | build(cuda): harden cargo-cuda.ps1 toolchain discovery | **not applicable** | No absorbed production path; touched `scripts`. |
| `b8dff972` | 2026-07-18 | perf(sextant): vectorize DiskANN i8 candidate scoring | **applicable** | Runtime delta in absorbed `sextant`; native semantic comparison/port and manual FSV remain required. |
| `22e63450` | 2026-07-18 | docs(law): close first-inference resource boundaries | **not applicable** | Test/benchmark/documentation/formatting-only intent is excluded by the absorption and zero-test policies. |
| `3739cbcb` | 2026-07-18 | docs(law): close sealed single-pass ingest | **not applicable** | Test/benchmark/documentation/formatting-only intent is excluded by the absorption and zero-test policies. |
| `679e8536` | 2026-07-18 | perf(law): bound judge association point reads | **not applicable** | No absorbed production path; touched `cli`, `docs`. |
| `e120057d` | 2026-07-18 | feat(law): gate canonical rebuild headroom | **applicable** | Runtime delta in absorbed `aster`; native semantic comparison/port and manual FSV remain required. |
| `2e02d8ce` | 2026-07-18 | feat(law): bound CourtListener locator audits | **not applicable** | No absorbed production path; touched `cli`, `docs`. |
| `7e16fc29` | 2026-07-18 | feat(law): separate case and opinion-part citations (#1853) | **not applicable** | No absorbed production path; touched `cli`, `docs`. |
| `1e28bc4b` | 2026-07-18 | fix(law): materialize typed summary attribution coverage (#1847) | **not applicable** | No absorbed production path; touched `cli`, `docs`. |
| `9b591c3e` | 2026-07-18 | feat(registry): TEI cross-encoder rerank runtime and lens runtime provenance | **applicable** | Runtime delta in absorbed `registry`; native semantic comparison/port and manual FSV remain required. |
| `f7519052` | 2026-07-18 | fix(assay): gate A37 admission namespace to passing gate evaluations only | **not applicable** | No absorbed production path; touched `cli`, `docs`. |
| `31240f99` | 2026-07-18 | fix(registry): attest physical TEI model and runtime identity (#1831) | **applicable** | Runtime delta in absorbed `registry`; native semantic comparison/port and manual FSV remain required. |
| `5bc40cec` | 2026-07-18 | Allow mixed learned lens template imports | **not applicable** | No absorbed production path; touched `cli`. |
| `e1e8767c` | 2026-07-18 | fix: honor frozen TEI batch limits (#1891) | **applicable** | Runtime delta in absorbed `registry`; native semantic comparison/port and manual FSV remain required. |
| `38f7458e` | 2026-07-18 | fix: copy DB-native A37 authority into vaults (#1826) | **not applicable** | No absorbed production path; touched `cli`. |
| `4dfecdb6` | 2026-07-18 | fix: refuse collapsed multi-vector assay evidence (#1892) | **not applicable** | No absorbed production path; touched `BUILDING_ON_CALYX.md`, `cli`, `docs`. |
| `1ce8ca35` | 2026-07-18 | fix(aster,weave): stop materializing every MVCC row on vault open; adopt in weave-loom (#1862) | **applicable** | Runtime delta in absorbed `aster`; native semantic comparison/port and manual FSV remain required. |
| `f58e6365` | 2026-07-18 | fix(lodestar,cli): bound kernel-build/kernel-answer memory; stream panel index IO (#1863, #1864) | **applicable** | Runtime delta in absorbed `lodestar`; native semantic comparison/port and manual FSV remain required. |
| `4ee8dca2` | 2026-07-18 | fix(search): bound MaxSim serving working set; stop pinning whole lenses resident (#1845) | **applicable** | Runtime delta in absorbed `search`; native semantic comparison/port and manual FSV remain required. |
| `33f65a19` | 2026-07-18 | fix(search,probe-matrix): enforce caller top-k on final fused results (#1846) | **applicable** | Runtime delta in absorbed `search`; native semantic comparison/port and manual FSV remain required. |
| `ed9bed3d` | 2026-07-18 | fix(lodestar): teach summarize recall the sealed panel contract (#1860) | **applicable** | Runtime delta in absorbed `lodestar`; native semantic comparison/port and manual FSV remain required. |
| `37109555` | 2026-07-18 | fix(aster): bind Base page index freshness to Base CF content, not global ledger head (#1861) | **already adapted** | Base-page v4 freshness semantics were ported natively; Synapse also fixes selected-key tombstone disclosure (issue #1760). |
| `8d074eae` | 2026-07-18 | fix(core,cli,mcp): report missing reproduce evidence as CALYX_REPRODUCE_INSUFFICIENT (#1865) | **applicable** | Runtime delta in absorbed `core`; native semantic comparison/port and manual FSV remain required. |
| `97c62077` | 2026-07-18 | test(lodestar): update stale direct-hit ledger test to panel-native answer contract | **not applicable** | Test/benchmark/documentation/formatting-only intent is excluded by the absorption and zero-test policies. |
| `abc67ed1` | 2026-07-18 | test(lodestar): update spectral communities test to v2 assignment method string | **not applicable** | Test/benchmark/documentation/formatting-only intent is excluded by the absorption and zero-test policies. |
| `fe48a669` | 2026-07-18 | test(cli): add weave RSS-budget parse/refusal unit tests (#1862) | **not applicable** | Test/benchmark/documentation/formatting-only intent is excluded by the absorption and zero-test policies. |
| `4331f467` | 2026-07-18 | test(cli): repair all stale calyx-cli test targets; suite compiles and runs green (#1894) | **not applicable** | Test/benchmark/documentation/formatting-only intent is excluded by the absorption and zero-test policies. |
| `facaa4c5` | 2026-07-18 | test(cli): repair graph_csr and assay_stream_fbin fixtures to current contracts (#1894) | **not applicable** | Test/benchmark/documentation/formatting-only intent is excluded by the absorption and zero-test policies. |
| `54816106` | 2026-07-18 | test(cli): bind vault identity in resident-worker FSV fixture (#1894) | **not applicable** | Test/benchmark/documentation/formatting-only intent is excluded by the absorption and zero-test policies. |
| `711b2d86` | 2026-07-18 | Merge pull request #1896 from ChrisRoyse/claude/calyx-dev-bug-triage-8nr3ph | **not applicable** | Merge integration only; constituent commits are classified separately in this inventory. |
| `b14384ad` | 2026-07-18 | chore: restore canonical Rust formatting (#1900) | **not applicable** | Formatting-only Base-page follow-up; no runtime delta after the native v4 port. |
| `e0e4567f` | 2026-07-18 | fix: remove retired resident MaxSim scorer (#1901) | **applicable** | Runtime delta in absorbed `search`; native semantic comparison/port and manual FSV remain required. |
| `c7f9beba` | 2026-07-18 | fix: restore warnings-clean CLI contracts (#1903) | **not applicable** | No absorbed production path; touched `cli`. |
| `21b3ec6e` | 2026-07-18 | fix: make LAW reranker reject truncated meaning (#1902) | **not applicable** | No absorbed production path; touched `docs`, `infra`. |
| `e8d1553a` | 2026-07-18 | fix: tolerate only the TEI container creation phase (#1902) | **not applicable** | No absorbed production path; touched `infra`. |
| `0c00e792` | 2026-07-18 | fix: preserve exact TEI information bytes (#1902) | **not applicable** | No absorbed production path; touched `infra`. |
| `19dcc189` | 2026-07-18 | fix: bind TEI clients to no-truncate meaning (#1902) | **applicable** | Runtime delta in absorbed `registry`; native semantic comparison/port and manual FSV remain required. |
| `c8390910` | 2026-07-18 | fix: expose frozen TEI request identity (#1902) | **not applicable** | No absorbed production path; touched `cli`. |
| `df4da04b` | 2026-07-18 | docs: record Calyx client no-truncate FSV (#1902) | **not applicable** | Test/benchmark/documentation/formatting-only intent is excluded by the absorption and zero-test policies. |
| `d33141f9` | 2026-07-18 | fix: preserve full StaticLookup document meaning (#1904) | **applicable** | Runtime delta in absorbed `registry`; native semantic comparison/port and manual FSV remain required. |
| `34e1578e` | 2026-07-18 | fix: expose StaticLookup runtime identity (#1904) | **not applicable** | No absorbed production path; touched `cli`. |
| `06760f4d` | 2026-07-18 | docs: record StaticLookup full-document FSV (#1904) | **not applicable** | Test/benchmark/documentation/formatting-only intent is excluded by the absorption and zero-test policies. |
| `76eed053` | 2026-07-18 | fix: add full-document legal structure and raw TF lenses (#1886 #1887 #1905) | **applicable** | Runtime delta in absorbed `registry`; native semantic comparison/port and manual FSV remain required. |
| `d847a3ca` | 2026-07-18 | fix: make repeated lens explain a determinism gate (#1906) | **not applicable** | No absorbed production path; touched `cli`. |
| `8fcca974` | 2026-07-18 | docs: record manual determinism proof (#1906) | **not applicable** | Test/benchmark/documentation/formatting-only intent is excluded by the absorption and zero-test policies. |
| `75e8a64f` | 2026-07-18 | fix: execute frozen algorithmic lenses in migration and assays (#1905) | **applicable** | Runtime delta in absorbed `registry`; native semantic comparison/port and manual FSV remain required. |
| `5490533f` | 2026-07-18 | fix: preflight migration source before durable writes (#1905) | **not applicable** | No absorbed production path; touched `cli`. |
| `6bc3bfa7` | 2026-07-18 | docs: record real-runtime migration FSV (#1905) | **not applicable** | Test/benchmark/documentation/formatting-only intent is excluded by the absorption and zero-test policies. |
| `f35d2142` | 2026-07-18 | fix: preserve lens blocks in A37 conditioning (#1885) | **applicable** | Runtime delta in absorbed `assay`, `forge`; native semantic comparison/port and manual FSV remain required. |
| `72c1dcb0` | 2026-07-18 | docs: record logistic modularization FSV (#1910) | **not applicable** | Test/benchmark/documentation/formatting-only intent is excluded by the absorption and zero-test policies. |
| `ddb1e6da` | 2026-07-18 | fix: lift CUDA A37 feature ceiling (#1885) | **applicable** | Runtime delta in absorbed `forge`; native semantic comparison/port and manual FSV remain required. |
| `929a4ee8` | 2026-07-18 | fix: normalize total lens variation in A37 (#1885) | **applicable** | Runtime delta in absorbed `assay`; native semantic comparison/port and manual FSV remain required. |
| `320cb5db` | 2026-07-18 | fix: isolate A37 planted calibration signal (#1885) | **applicable** | Runtime delta in absorbed `assay`; native semantic comparison/port and manual FSV remain required. |
| `e9af7df3` | 2026-07-18 | fix(assay): require converged lens-block logistic fits (#1885) | **applicable** | Runtime delta in absorbed `assay`, `forge`; native semantic comparison/port and manual FSV remain required. |
| `edbfff6a` | 2026-07-18 | fix(gpu): enforce host-wide reservations (#1890) | **already adapted** | Host-wide GPU reservation semantics were adapted in Synapse commit `1ba6e364` (issue #1760). |
| `e455d0f6` | 2026-07-19 | fix(cli): include GPU reservation in request fixture (#1917) | **not applicable** | No absorbed production path; touched `cli`. |
| `71397143` | 2026-07-19 | fix(cli): align migration fixtures with real runtimes (#1918) | **not applicable** | No absorbed production path; touched `cli`. |
| `773a370c` | 2026-07-19 | fix(cli): carry conditioning-v4 fixture provenance (#1919) | **not applicable** | No absorbed production path; touched `cli`. |
| `6edd0449` | 2026-07-19 | fix(cli): narrow probe readback path contract (#1920) | **not applicable** | No absorbed production path; touched `cli`. |
| `f384490f` | 2026-07-19 | Modularize CLI command facade (#1915) | **not applicable** | No absorbed production path; touched `cli`. |
| `313860bc` | 2026-07-19 | Modularize algorithmic registry runtime (#1916) | **applicable** | Runtime delta in absorbed `registry`; native semantic comparison/port and manual FSV remain required. |

## Reproducibility and acceptance boundary

The coverage count is independently reproducible with:

```powershell
git -C C:\code\Calyx-Dev rev-list --count 9894f84f..313860bc
```

Expected result for the recorded head is `283`. The commit ledger is an inventory of source-history reality. It does not claim that the 135 applicable commits are shipped or manually verified. Issue #1760 remains open until each applicable row receives an evidence-backed terminal decision and every reachable native port has D1 manual FSV evidence.

