# Synapse/Calyx final integration sweep — 2026-08-20

## Verdict

The remaining Synapse issue surface is implemented on `main` and manually
verified through the real strict Codex MCP client plus independent physical
Source-of-Truth reads. The Calyx integration is not a disconnected analytics
feature: exhaustive typed causal evidence is durably published in `Graph`,
read through the existing `storage intelligence` facade, consumed by agent tool
steering, exposed by readiness/health, and bounded by honest identification,
resource, freshness, and provenance contracts.

No automated test, benchmark, FSV driver, harness, CI job, GitHub Action, mock
row, fallback estimator, sampled pair set, alternate database, branch, worktree,
or alternate Cargo target directory was used as acceptance evidence. Format,
compile, and warning-denying lint are structural checks only.

## Exact final runtime precondition

The standard `setup operation=repair` transaction built, candidate-verified,
installed, and started the clean release from commit
`66b399e6887b29414f34f45be7e32acda5bcc84f`.

```text
installed executable: C:\Users\hotra\.cargo\bin\synapse-mcp.exe
installed bytes:      263,884,617
installed SHA-256:    13840B79A51C1ECB74118ECD384155454D3B5C84A345ADEAF02721D2A94C9698
daemon PID/listener:  48780 / 127.0.0.1:7700
build profile:        release + calyx-cuda
build tree:           clean; changed input count 0
health:               ok=true, build=66b399e6887b
strict public tools:  40
tool-surface SHA-256: 24a072513372f2a2093e9a4d67c485f5285592fcf7465d7101a1671e7ed08034
vault id/path:        01KXPT5BE11Y45CXD4D90CTX7D / %LOCALAPPDATA%\synapse\db-daemon
math/assay backend:   CUDA / NVIDIA GeForce RTX 5090
```

The caller-session attestation in `CF_SESSIONS` matched the live sanitized
`tools/list`, named `codex-mcp-client` and protocol `2025-06-18`, and recorded a
subsequent tool call. This is the schema-valid production client path; direct
HTTP/stdio callers were not substituted.

The final setup repair Source of Truth is
`%LOCALAPPDATA%\synapse\setup-repair-runs\repair-53380-1787247282883\repair-run.json`:
state `completed`, exit `0`, 13,776 bytes, SHA-256
`DFA2B1E0B356BF672F4FC806D7BB5AF092F9592B8C8706F78A5A239C55E951B0`;
its owned child PID 50096 is absent. The Chrome activation checkpoint remains a
terminal schema-v4 record (`completed`, generation
`setup-52096-9ceb0c3be6774d458e08fbad5a7c4f1b`, 31,379 bytes, SHA-256
`04D7CA546A7A3D3FD38975A7D2BF906C3CD7EBFE011251A4D8396D88BADD6094`).

## Exhaustive causal maps: producer, durable bytes, and consumer

### Full action map

The strict trigger was `storage operation=intelligence` with
`operation=causal_map_read`, panel `2185002`, group key `action_kind`, 60-second
bins, lag 8, FDR alpha `.05`, record bound 20,000, and the exact six-hour event
window `[1786918895909000000,1786940495909000000)`.

The returned typed artifact was `synapse.calyx.causal_map.v3`, evidence class
`observational_predictive`, `structural_effect_identified=false`, with all 22
streams and all `C(22,2)=231` pairs loaded and enumerated. Its independent
Aster snapshot read proved:

| Graph row | Key hex | Bytes | SHA-256 | Physical/logical |
|---|---|---:|---|---|
| content-addressed artifact | `47434d50310021572a33d339b029ee059b607e3f4002157f85` | 4,129,539 | `33d339b029ee059b607e3f4002157f85adb6e4b8e418df23374c9dded936d527` | true / true |
| normalized-scope pointer | `47434d49310021572a415cad90413aee704c3c47adfb5ccd5a` | 793 | `5bf4850991eb7acd79fbe8ca1d6e6b282d02b96a3890a354c5eeb2e4a4e21d02` | true / true |

Snapshot lease 5185 pinned sequence 4,339,074, read both exact rows, and was
released to `active_lease_count=0`. The map preserves separate evidence lanes;
no aggregate “causal score” flattens their assumptions:

- PC-stable conditional-independence skeleton;
- Gaussian partial-correlation network;
- transfer entropy in both directions across lags;
- Granger tests in both directions across lags;
- signed lag/cross correlation;
- convergent cross mapping (CCM);
- temporal cross-K;
- fixed-decay exponential Hawkes branching;
- four named, family-local Benjamini-Hochberg FDR corrections.

Estimator inability is typed data, never substitution. Each lane carries its
assumptions/result or an exact failure. Resource accounting proves the declared
stream/pair/lag/conditioning cardinalities fit before compute; an over-budget
request refuses rather than sampling or omitting associations. The data-
processing/identification boundary remains explicit: estimator agreement over
observational timing does not manufacture an intervention contrast or erase
confounding.

### MCP tool-steering integration

A fresh strict `agent operation=recommend_tools` call for task class
`issue-1682-final-causal-integration-fsv` consumed the rolling MCP-usage causal
map through `synapse.steering.causal_map_context.v1`:

```text
context status:       physically_verified
panel/group:          syn-mcp-usage-v1:1965007 / mcp_usage_tool
coverage:             15 streams / 105 expected pairs; all rows and pairs complete
window records/bins:  328 / 345
evidence class:       observational_predictive
structural effect:    false
BH families:          Granger, cross-correlation, PC-stable, partial correlation
decision grounding:   provisional_insufficient_evidence (zero task outcomes)
```

That truthful provisional result is success: causal context informs ranking but
is never counted as an empirical task success or action authorization. The
Hawkes global lane reported its exact insufficient-sample failure for the
one-event `agent` stream while PC-stable and partial correlation remained
measured; no fallback or hidden omission occurred.

The call persisted
`CF_KV steering/v1/decision/tool/1787248113796241000/698fbb8c2e6afbd913e77d6ed54df2e2affce316bf1180f2610993421e92518c`.
Separate snapshot lease 5388 found the row physical and logical at sequence
4,339,104: 45,797 bytes, SHA-256
`5c433f60136d1a055b637cc7a8a2cc41c5c46791bf868ef5a041692f4ee636b5`,
exactly matching the consumer response. Release returned the reader count to
zero.

### Causal performance and boundaries

The complete 20-stream/190-pair production workload improved from 395,895 ms
to 18,552 ms (21.34x) after one synchronous computation began owning one
per-vault CUDA context/lease instead of recreating an accelerator session per
pair. Every pair and artifact byte was preserved. The physical GPU reservation
ledger was empty after release. Full timings and lifecycle evidence are in
`issue-2245-causal-cuda-context-20260816.md`.

Strict edge calls separately read the serving Graph pointer before and after:

| Edge | Exact refusal | Durable pointer after |
|---|---|---|
| empty group key | `TOOL_PARAMS_INVALID` | unchanged |
| lag `0` | `TOOL_PARAMS_INVALID`, valid `1..=32` | unchanged |
| one-sided pair scope | `TOOL_PARAMS_INVALID` | unchanged |
| FDR alpha `1.0` | `TOOL_PARAMS_INVALID`, requires `(0,1)` | unchanged |
| older source frontier | `SYNAPSE_CALYX_CAUSAL_MAP_POINTER_REGRESSION` | current generation retained |

## Critical Calyx capability surface

The complete route/producer/physical-evidence matrix is in
`issue-2245-critical-capability-surface-20260817.md`. The closing readback keeps
all capabilities on the existing 40-tool facade:

| Capability family | Production routes | Physical Source of Truth |
|---|---|---|
| Aster WAL/MVCC/retention/GC/backup | `storage summary`, snapshots, GC, backup/restore | vault WAL/SST/manifest rows and reader-lease registry |
| panels/lenses/lifecycle | `storage panel_lifecycle`, coverage, temporal panels | Registry, Base, slot CFs, Ledger |
| associations/Assay | `storage intelligence` weave/bits/sufficiency/redundancy/synergy/causality/maps/periodicity/drift/hazard | XTerm, Graph, Assay, Base, Anchors |
| kernel/answers | `hygiene kernel*`, `find` | Kernel CF plus immutable search artifacts |
| guard/quarantine | `hygiene guard_calibrate/guard_verify`, guarded consumers | Guard profile + trusted-exemplar generation + Ledger |
| oracle/readiness | `storage intelligence oracle_*`, `routine`, `assist`, `health` | AnnealReport, Kernel, Guard, Ledger, exact cohorts |
| search/fusion | `find similar`, `storage search_rebuild/find_similar` | immutable DiskANN/BM25/MaxSim/SPANN manifests and sidecars |
| provenance/reproduction/erase | `audit`, `privacy`, snapshots | append-only Ledger, tombstones, exact source rows |
| reversible optimization | `hygiene anneal_*` | Anneal artifacts, rollback rows, generation pointers |
| grounded consumers | `agent recommend_tools`, `assist`, `routine` | CF_KV decision rows, causal Graph generation, routine state |

The bounded Base walk examined 2,311,265 real rows with exact accounting and a
958,324,736-byte internal private-memory peak; the unattended pass peaked at
994,258,944 bytes, both below 1 GiB. No record sampling or RAM-limit fallback
was introduced. Details and edge reads are in
`issue-2245-bounded-calyx-read-admission-20260815.md`.

## Readiness-gated autonomy is honest

Final `health` independently read the persisted schema-v4
`synapse.action` validation cohort:

```text
source rewards:       261
eligible causal rows: 120, SHA b8b55c0f...c19681
excluded incomplete:  141, SHA 3b3e051a...a0882
population equality:  261 = 120 + 141
panel bits / entropy: 0.488380432 / 0.735508561
named deficit:        0.285359 bits for causal lens 7414a225...6845d
kernel:               absent for exact panel/slot/domain
Goodhart:             1 violation; in-region fraction 0
recurring mistakes:   5 / 24 chronological holdout actions
readiness verdict:    not_ready
```

Evidence integrity, scope, population accounting, Ledger binding, corpus
freshness, Guard freshness, and evidence lease all passed. Synapse correctly
does not arm autonomy while sufficiency, kernel, Goodhart, and mistake-closure
tiers fail. The readiness gate is a capability precisely because it refuses;
changing `not_ready` to success would cover up real missing grounded evidence.
The arming policy and its three no-mutation boundary cases are recorded in
`2026-08-06-issue-1690-autonomy-arming.md` and issue #1690.

## Browser/foreground, bridge state, and setup state

The normal Chrome bridge manifest contains no `debugger`, `nativeMessaging`,
or `management` permission. Its installed manifest is 998 bytes, SHA-256
`7EB4C64AEFA29BB01DB42EDA935EE18A198A2BC7EAA7A610D8557918D571369F`;
the v26 worker SHA-256 is
`CFE7B96AAC0DB4A6768D2478903AB9A8D2E950781D2178C424A4C3AFACF6F69E`.
The dormant bundle may contain debugger-lane source, but Chrome does not grant
that API to the normal extension and the daemon routes raw CDP only through the
separate debugger profile/endpoint contract.

Happy-path strict MCP proof used an isolated normal extension host with
`cdp_debug=false` and no remote-debugging switch:

1. `browser_tabs list` returned the extension-owned tab without using the human
   OS foreground.
2. `browser_tabs new` created a known background target.
3. `browser_nav navigate` moved it to `https://example.com/`.
4. A separate `browser_dom content` read returned the exact 544-byte Example
   Domain document, including its title, H1, and IANA link.
5. `browser_tabs close` removed the target; a separate list proved it absent.

The human foreground stayed `World of Warcraft`, HWND 125963510 / PID 71504,
and the cursor stayed `(1935,1105)`. Human Chrome root PID 18816 stayed alive.
The normal host refused raw debugger evaluation with
`A11Y_CDP_EXTENSION_UNAVAILABLE`, proving it never attached a debugger.

Three navigation edges retained the exact 544-byte DOM before/after: empty URL
(`TOOL_PARAMS_INVALID`), nonexistent target (`ACTION_TARGET_INVALID`), and raw
debugger request (`A11Y_CDP_EXTENSION_UNAVAILABLE`). Three final process-launch
edges on release `66b399e6` also proved zero OS processes and zero
`CF_PROCESS_HISTORY` rows before/after:

| `cdp_debug=false` input | Exact causal refusal |
|---|---|
| includes `--remote-debugging-port` | `cdp_disabled_with_debug_switches` |
| two `--user-data-dir` switches | `duplicate_cdp_switch` |
| empty `--user-data-dir=` | `cdp_switch_value_empty` |

The shared validator now runs before generic popup policy and again at launch
construction, so malformed requests cannot be misdiagnosed or cross an
authority/resource/spawn boundary. The dedicated FSV roots and their exact
r2/r3/r4 profiles were then terminated and removed; ports 9231/9235 have no
listener, the CFT runtime/profile directories are absent, PID 18816 remains
alive, and only the intended daemon owns port 7700. With no active Profile-5
window after cleanup, live bridge health truthfully reports
`no_active_chrome_bridge_host`; setup treats that state as an explicit skipped
activation precondition, not a silent alternate transport. Installed bytes and
the completed activation checkpoint remain the durable readiness truth.

## Durable browser-owner defects

- **#2223 schema-v5 migration:** a real quiescent schema-5 ledger at revision 41,
  terminal sequence 738 (711 bytes, SHA-256
  `98a65c9ce47c65e53c13cdeb3d73f6c13389c74a53013a01add306371308902b`)
  migrated through strict `act operator_panic_status` to schema 7 / revision 44
  / sequence 739 with zero terminal outbox. Separate `chrome.storage.local`
  readback found the canonical row and exact archived schema-5 bytes. Empty
  missing-session state rotated owner identity; nonquiescent v5, browser-session
  digest mismatch, and future schema all refused with canonical/archive bytes
  unchanged.
- **#2225 bounded error causality:** one 134-character primary durable-state
  error remained byte-stable while three reconnect/navigation occurrences
  incremented only the bounded secondary diagnostic counter. Admission remained
  closed and status reads did not mutate the failed row. The issue evidence
  retains the complete digest rather than abbreviating it here.
- **#2161 reversible page instrumentation:** public clock uninstall removes the
  current-page owner and its exact `Page.addScriptToEvaluateOnNewDocument`
  session-owned identifier. Empty owner, lower/upper time boundaries, owner
  collision, callback self-clear, and navigation persistence were manually
  exercised; current/future descriptors and owner registries were separately
  read before/after. No cross-session identifier guess or shim resurrection is
  allowed.
- **#2220 setup checkpoint:** successful activation writes terminal schema v4,
  while missing source, host-unavailable, daemon-PID drift, malformed JSON, and
  obsolete checkpoint generations fail with exact remediation and no false
  completion. The final repair manifest, binary, process, socket, extension
  bytes, checkpoint, and released maintenance lock were separately read.

## Research applied after diagnosis

Exa MCP was explicitly attempted during this work; the account returned HTTP
402 credit exhaustion, which is recorded rather than hidden. Independent
primary-source research was completed through the native web lane:

- Chrome Extensions service-worker lifecycle and `chrome.storage` support
  durable state outside an ephemeral MV3 worker.
- Chrome `runtime.OnInstalledReason` distinguishes install from update, which
  is why install rotates logical owner generation while a validated update may
  preserve continuity.
- Chromium CDP documents `Page.addScriptToEvaluateOnNewDocument` and its
  identifier-based removal; implementation inspection confirmed identifiers
  belong to the originating session.
- Tokio's fair semaphore and blocking-task contracts support one explicit
  completion-waiting background whole-corpus lane and bounded foreground
  admission, rather than overlap or timeout inflation.
- RocksDB iterator/snapshot guidance supports one allocation-reusing forward
  walk over an explicit pinned snapshot rather than repeated page materialization.

Exact source links and how they constrained implementation are retained in the
causal, bounded-read, critical-surface, ADR, and issue-specific evidence files.

## Evidence index

- `issue-2245-critical-capability-surface-20260817.md`
- `issue-2245-causal-map-integration-20260815.md`
- `issue-2245-causal-cuda-context-20260816.md`
- `issue-2245-bounded-calyx-read-admission-20260815.md`
- `issue-2245-mvcc-reader-observability-20260815.md`
- `2026-08-05-issue-1689-grounded-steering.md`
- `2026-08-06-issue-1690-autonomy-arming.md`
- `../adr/2026-08-15-exhaustive-typed-causal-evidence-maps.md`

## Structural gates

Before the final push, the root Synapse and absorbed `calyx/` workspaces were
checked with their own `cargo fmt --all --check`, `cargo check --workspace
--all-targets`, and `cargo clippy --workspace --all-targets -- -D warnings`,
plus `git diff --check`. These prove build/format/lint conformance only; the MCP
triggers and physical readbacks above are the behavioral verdict.
