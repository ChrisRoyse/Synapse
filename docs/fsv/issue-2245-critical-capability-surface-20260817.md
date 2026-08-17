# Issue #2245 — complete critical Calyx capability surface (2026-08-17)

## Verdict

Every critical capability named by `docs/calyx/INTEGRATION_PLAN.md` has a
schema-valid route through the 40-tool production facade, and its physical
producer/reader path has manual evidence. This closing pass found and repaired
one live defect rather than certifying the route map from declarations alone:
scheduled vault verification used the finite foreground admission contract and
therefore cancelled its fair wait whenever another legitimate whole-corpus
maintainer owned the lane.

Commit `6b1fd76c` makes the caller class explicit:

- `periodic_vault_verify` uses completion-waiting background admission;
- every MCP facade whole-corpus call uses the explicitly named foreground API;
- ambiguous generic wrappers are private, so a future scheduler/foreground
  classification error fails at compile time;
- one fair exclusive whole-corpus lane remains authoritative. No retry lane,
  overlap, timeout increase, partial verdict, or fallback was introduced.

## Exact installed reality and strict-client precondition

The standard installer built and handed off the exact release before stopping
at the unrelated fail-closed Chrome background-reload checkpoint. No human
Chrome window was focused, restarted, closed, or altered.

```text
checkout / build commit:  6b1fd76ceadf2ed8977efc354da7b5ce082b992a (clean when built)
daemon PID / parent:      39740 / 47936
listener:                 127.0.0.1:7700 owned by PID 39740
executable:               C:\Users\hotra\.cargo\bin\synapse-mcp.exe
binary bytes:             263,694,153
binary SHA-256:           15BB84F8759FB6CB2C50AF854E18F4CB0BCD20D93E8A9DF86CA1FE736144FD7F
release feature:          calyx-cuda
health:                   ok=true, build=6b1fd76ceadf
strict tool count:        40
strict tool surface:      ecfc7ae5004a8e40b6df0987bd2dc18ecce160ad8cd86654f7287c974610bcfb
strict MCP session:       ef5a5d00-8357-4dd0-a9d0-f11ea2b8562f
client / protocol:        codex-mcp-client 0.147.0 / 2025-06-18
caller tools/list state:  caller_session_attested_current; subsequent tools/call observed
profile / input lease:    normal_agent / not held
vault:                    01KXPT5BE11Y45CXD4D90CTX7D
```

The caller-session attestation physically resides at
`CF_SESSIONS mcp/tool-surface-attestation/v1/<session_id>` and matches the live
sanitized `tools/list`. This is the real wired strict client; no direct HTTP,
stdio helper, CLI substitute, or schema-bypassing caller was used as a trigger.

## Complete capability route and evidence matrix

The facade contract was read from the live daemon after installation. Historical
evidence below remains the accepted manual physical proof for unchanged
producers; this pass re-read representative live artifacts across the whole
stack rather than rebuilding isolated fixtures.

| Calyx capability | Production MCP route | Physical Source of Truth and proof |
|---|---|---|
| Aster vault, WAL, MVCC, retention, GC, backup/restore | `storage summary`, `storage snapshot_open/read/release`, `storage snapshot_gc_status`, `storage backup/backup_status/restore_verify` | Current vault summary read exact CF counts/sizes and the Calyx backend; bounded reader leases were opened, point-read, released, and returned to zero. Prior complete vault/cutover/backup proof: `issue-1653-calyx-vault-20260715.md`, `issue-1656-calyx-kv-backend-20260716.md`, `issue-1657-calyx-retention-parity-20260716.md`, `issue-1658-calyx-pressure-parity-20260716.md`, `issue-1659-calyx-gc-parity-20260716.md`, `issue-1660-storage-inspect-dump-calyx-20260716.md`, `issue-1661-storage-migrate-final-fsv-20260716.md`, `issue-1662-calyx-cutover-20260717.md`, `issue-1687-vault-backup-restore-20260729.md`, and `issue-2245-mvcc-reader-observability-20260815.md`. |
| Registry panels and lifecycle | `storage panel_lifecycle`, `storage temporal_panels`, `storage panel_coverage` | Exact Registry rows, revisions, slot/lens identities, and lazy-backfill counts are proven in `issue-1668-panel-lifecycle-registry-foundation-20260804.md` and `issue-1668-panel-lifecycle-end-to-end-20260804.md`. Current reads honestly returned absent Registry rows for undeclared versions rather than fabricating lifecycle state. |
| Forge runtime and admission | `health` Calyx vault/math fields; Assay and search callers consume it | Current health read `calyx_assay_compute_backend=cuda`, `calyx_math_backend=cuda`, device `NVIDIA GeForce RTX 5090`, and `dormant_verified`; dispatch buffers and the host reservation ledger returned to zero after use. Prior math and resident gather proof: `issue-1654-calyx-math-backend-health-20260716.md`, #2147, and `issue-2245-causal-cuda-context-20260816.md`. |
| Loom associations and reactive delivery | `storage intelligence weave`, `hygiene blind_spot/drift`, `subscribe` | Current derived state has zero weave backlog for panel 1963001 and exact global XTerm/Graph readbacks. Complete association/reactive proof: `issue-1671-1996-1997-scheduled-weave-20260804.md` and all `issue-1680-*.md` evidence. |
| Assay bits, sufficiency, redundancy, synergy, periodicity, hazard, drift, causal maps | `storage intelligence` typed sub-operations | Current action causal artifact is a complete v3 20-stream/190-pair generation carrying PC-stable, partial correlation, transfer entropy, bidirectional Granger, signed lag correlation, CCM, cross-K, Hawkes, and family-local BH-FDR; it remains `observational_predictive` and `structural_effect_identified=false`. Physical Graph proofs: `issue-1672-1998-known-mi-20260804.md`, `issue-1674-planted-blind-spot-20260804.md`, `issue-2245-causal-map-integration-20260815.md`, and `issue-2245-causal-cuda-context-20260816.md`. |
| Lodestar kernel and grounded answers | `hygiene kernel/kernel_rebuild/grounding_gap`; `find` answer paths | A live kernel read for panel 1964001 / slot 113 returned kernel id `1a92…`, 1,249 members, held-out recall `1.0` against target `.95`, and 200 held-out queries. A separate grounding-gap read reported 61 grounded / 39 ungrounded of 100, explicitly non-provisional. Full producer/answer proof: `issue-1675-1999-kernel-recall-answer-20260804.md` and `issue-2245-bounded-calyx-read-admission-20260815.md`. |
| Ward conformal guard, identity lock, quarantine | `hygiene guard_calibrate/guard_verify`; guarded `find`; `routine` identity paths | This pass calibrated the live MCP-usage panel from 1,000 real grounded records and then physically read the Guard profile, serving generation, and exact Ledger verdict row; details are below. Prior identity/quarantine proof: all `issue-1677-*.md` evidence. |
| Oracle prediction, reverse, completion, next occurrence, readiness | `storage intelligence oracle_*`, `routine`, `assist`, `health` | Current health serves a persisted readiness report and honestly returns `not_ready`: panel bits `.215963751 < .678739250`, missing action kernel/guard/held-out validation, plus an exact cheapest fix. It does not turn absence into a confident answer. Full prediction/completion/readiness proof: all `issue-1678-*.md` evidence. |
| Ledger verification, provenance, reproduce, erase | `hygiene vault_verify`; `audit verify_chain/reproduce`; `privacy erase`; `storage snapshot_read` | This pass proves scheduled verification convergence and exact Ward verdict sequence 1,525,304. Prior reproduction/erase proof: `issue-1679-reproduce-erase-20260804.md`; scheduled verification history: `issue-1679-scheduled-vault-verification-20260804.md`. Generic answer replay remains intentionally absent unless the complete frozen query/candidate/index/tuning universe is persisted; the public route never relabels provenance-binding reproduction as answer replay. |
| Anneal reversible optimization | `hygiene anneal_status/anneal_search_propose/anneal_rollback`; persisted generation consumers | Current status independently read artifact SHA `f27f4058998e32f90ba18253d0de8e51c506575309dd1c449c480e4a4b5a3634`, 386 bytes, three rollback rows, two changes, fusion `K=60`, and index knobs. Full promotion/rollback/search proof: all `issue-1681-*.md` evidence. |
| Sextant/Search fused retrieval | `find similar`; `storage find_similar/search_rebuild` | Current strict searches returned three grounded hits for panel 1963001 and three for panel 1964001. Panel 1964001 reported manifest SHA `fa019ea22c74929b2baa5e806956e5b6c123af055c4c11716f3996fb4cf0df70`; a separate filesystem hash of `idx/search/panel_0001964001/manifest.json` matched exactly (5,488 bytes). Full fused/temporal proof: `issue-1676-fused-find-temporal-20260804.md` and Anneal search evidence. |
| Native TimeSeries and OLAP | `cost summarize/rollup_backfill`; `telemetry status` | Current cost summary declared native TimeSeries rollup point reads and zero transcript scans while reporting 7 spawns, 1 model, and 499,567 tokens. Telemetry reported native Aster TimeSeries as its source. Full physical proof: `issue-1688-native-timeseries-olap-20260804.md`. |
| Grounded steering consumers | `agent recommend_tools`; `assist`; `routine` | The current steering path independently consumes the exhaustive MCP causal map with every estimator lane and keeps it observational-only. Persisted decision proof is in `2026-08-05-issue-1689-grounded-steering.md`, `2026-08-06-issue-1690-autonomy-arming.md`, and the consumer section of `issue-2245-causal-cuda-context-20260816.md`. |

## Root defect: scheduled vault verification admission

### Source of Truth

- Trigger/process: durable daemon log
  `%LOCALAPPDATA%\synapse\logs\daemon-stderr-gen1-20260817021100.log`.
- Outcome: live `health.subsystems.vault_verify`, the Ledger verification bounds,
  and the same durable admission/completion records.
- Concurrency invariant: one exclusive whole-corpus owner. Foreground requests
  have a 1,000 ms admission budget; background maintainers wait fairly to
  completion.

### Before

The prior exact daemon logged:

```text
STORAGE_MAINTENANCE_BUSY requested_operation=periodic_vault_verify
active_operation=storage_derived_state active_for_ms=150999
wait_budget_ms=1000 foreground_work_dispatched=false
VAULT_VERIFY_PERIODIC_TASK_FAILED
health.vault_verify.status=error / verdict unreadable
```

Source inspection found the scheduler calling the foreground wrapper. Three
falsifiable alternatives were rejected: the verifier itself was not corrupt,
the whole-corpus permit was not leaked, and Ledger data was readable after the
owner completed. The caller classification alone explained the exact 1,000 ms
cancellation.

### Happy path after the fix

The new daemon started a real derived-state owner, then the scheduled verifier
became due while that lane remained occupied. It did not cancel or overlap:

```text
derived owner admitted:        2026-08-17T07:16:28.648011Z
derived owner completed:       2026-08-17T07:19:46.826917Z (198,155 ms)
periodic verifier admitted:    admission_wait_ms=68,543
                               lane_occupied_at_request=true
                               active_after=1, max=1
periodic verifier completed:   exec_ms=7,941, is_ok=true
vault id:                      01KXPT5BE11Y45CXD4D90CTX7D
scan:                          incremental_tail [1,521,114, 1,525,210)
Ledger head:                   1,525,210
tip hash:                      e42f57bd34d8a8f4beaeca77dff732b61bb7c45896e773a7a157d779bed9b3c3
raw commitments:               intact
```

A separate strict `health` call returned `status=ok`, `verdict=verified`, the
same bounds/head, and `scheduled=true`.

The foreground contract remained bounded: a real strict
`hygiene guard_verify` during another derived-state owner refused after 1,000 ms
with `STORAGE_MAINTENANCE_BUSY`, named the active owner and elapsed tenure, and
logged `foreground_work_dispatched=false`. The owner later completed normally.

### Invalid boundaries

Each strict request was followed by a separate health/log read; no verifier was
admitted, no partial verdict replaced the verified state, and the health bounds
remained unchanged.

| Request | Result | Physical state after |
|---|---|---|
| `tail_entries=0` | `TOOL_PARAMS_INVALID`, outside `1..=1,000,000` | verified tail/head unchanged; no maintenance admission |
| `tail_entries=1,000,001` | same exact range refusal | unchanged |
| unknown field `bogus` | `TOOL_PARAMS_INVALID`; accepted fields named | unchanged |

## Ward live calibration and verdict proof

### Calibration trigger and immutable serving rows

The selected panel was the current MCP-usage generation `1965007`, whose
grounding-gap read found 500/500 grounded records. Slot 115 is its 128-dimension
dense record vector. A 500-record dry calibration correctly refused because it
contained only 46 bad examples, below Ward's fixed minimum 50; nothing was
synthesized or the minimum lowered. A 1,000-record dry pass measured 896 good,
104 bad, target FAR `.03`, and `tau=.940462589` with zero empirical bad accepts.

The persistent real MCP transaction was performed only while the same session
held the explicit foreground lease and `break_glass` profile. A `finally`
de-escalation restored `normal_agent` and released the lease; independent
`profile status` and `act lease_status` proved both outcomes.

```text
operation:             hygiene guard_calibrate
panel / slot:          1965007 / 115
aspect / domain:       content / synapse.mcp_usage
alpha / target FAR:    .05 / .03
real record bound:     1,000
failure disposition:   reject_closed
blocking execution:    1,611 ms, is_ok=true
lowered generation:    38 with recorded source_ledger_seq field 4,204,318
Guard profile key:     70726f66696c650070616e656c00001dfbcf
profile bytes / SHA:   711 / 1c821eb98dcd3fb3c8851298c7890b96c1cd547b0989f984832b2883b8317c9d
Guard serving key:     73657276696e670070616e656c00001dfbcf
serving bytes / SHA:   491,150 / b265415723bfcb4604791db53384615c3a3ca4f097afca5791c866abda7aed36
```

The keys were point-read through an independent Aster snapshot, not inferred
from the calibration response.

### Happy verification

After separately reading both Guard rows before the trigger and releasing its
snapshot, the strict client called:

```text
hygiene guard_verify {
  panel_version: 1965007,
  query_cx_id: "000120ad1e9b343a4eb3408495be8521",
  high_stakes: false
}
```

The typed result was non-provisional and passed every required slot:

```text
guard id:              d0c4c1d8-a5de-4ef4-a063-a44a70d03c56
trusted exemplars:     896
slot / cosine / tau:   115 / 1.0 / 0.940462589263916
overall / policy:      true / all_required
calibration FAR/FRR:   0 / 0
Ledger seq/hash:       1,525,304 / 6eb508a1ca546aa69a28afede555443247262c6edceb77ddb2d9eedf1509b41a
```

A new Aster snapshot then physically found:

```text
profile row:  present, 711 bytes, SHA 1c821eb9...17c9d (unchanged)
serving row:  present, 491,150 bytes, SHA b2654157...aed36 (unchanged)
Ledger key:   0000000000174638
Ledger row:   physical=true, logical=true, 379 bytes
payload SHA:  9f670e2535d065c95cf682ecdbf6a446dcded9fe7f2c8135842d5cc31a4c0531
reader lease: 27817 released; active leases=0
```

The Ledger payload SHA is a byte hash of the encoded row; the returned
`ledger_hash` is its verified chain identity. They are intentionally different
measurements.

### Three fail-closed verification edges

For each edge, the prior case's separate after-read was also the next case's
before-read. Every before/after read found the same serving key, length, and SHA;
each snapshot lease was separately released to active count zero.

| Edge trigger | Structured refusal | Guard serving before -> after |
|---|---|---|
| empty `query_cx_id=""` | `SYNAPSE_CALYX_CX_ID_INVALID`, expected 32 characters, got 0 | 491,150 bytes / `b2654157…aed36` -> identical |
| 32 non-hex `z` characters | `SYNAPSE_CALYX_CX_ID_INVALID`, invalid hex byte at index 0 | identical -> identical |
| unknown field `bogus=true` | `TOOL_PARAMS_INVALID`, exact accepted field list returned | identical -> identical |

Additional honest boundaries observed during calibration were: panel 1964001
had no declared adjudication polarity (`SYNAPSE_CALYX_GUARD_BAD_CORPUS_ABSENT`),
500 records had only 46 bad examples
(`SYNAPSE_CALYX_GUARD_BAD_CORPUS_INSUFFICIENT`), and persistence under
`normal_agent` was denied with `TOOL_PROFILE_POLICY_DENIED`. No fallback corpus,
manufactured labels, silent clamping, or partial profile was published.

## Research applied

Diagnosis preceded research. Both Exa MCP and native web search were used. The
load-bearing concurrency guidance came from Tokio's official documentation:

- `Semaphore` is fair/FIFO; cancelling an acquire loses its place in the queue:
  https://docs.rs/tokio/latest/tokio/sync/struct.Semaphore.html
- blocking work belongs on `spawn_blocking`, and a started blocking task cannot
  be aborted as an ordinary async future can:
  https://docs.rs/tokio/latest/tokio/task/fn.spawn_blocking.html

These contracts support the implemented separation: completion-owned scheduled
work keeps its fair queue position, finite foreground requests retain a bounded
admission deadline, and the synchronous verifier runs off Tokio's async worker
threads.

## Structural checks and scope discipline

After the source edit:

```text
cargo fmt --all --check                                      PASS
cargo check -p synapse-mcp                                  PASS
cargo clippy -p synapse-storage -p synapse-mcp -- -D warnings PASS
```

These are compile/format/lint evidence only, never behavioral acceptance. No
automated tests, benchmarks, FSV drivers/harnesses/scripts, CI, GitHub Actions,
mock data, branches, worktrees, alternate target directories, side stores,
retries, fallbacks, or human-Chrome manipulation were used.

## Final conclusion

The strict public facade now exposes and physically proves every critical Calyx
capability declared by the integration plan. The audit's live failure was fixed
at its caller-classification boundary, and the closing Ward transaction proved
the final profile -> immutable serving generation -> per-slot verdict -> Ledger
chain. Honest insufficient/readiness states remain visible and actionable; they
are not disguised as capability outages or successful intelligence.
