# Issue #1691 FSV - Calyx-Steered MCP Surface

Date: 2026-07-18

## Root Cause

Synapse already emitted per-tool lifecycle telemetry in `daemon_lifecycle`, but the terminal event was not returned to the MCP handler and nothing persisted a durable, Calyx-measured usage corpus. That meant MCP calls had JSONL/process-local telemetry only, with no `CF_KV` usage row, no `syn-mcp-usage-v1` constellation, no grounded outcome anchor, no guide query, and no reversible promotion ledger. The system could observe calls after the fact but could not learn or enforce grounded next-best-call guidance from the same physical source of truth.

During FSV, promotion exposed a second defect: `guide_promote.promoted_value` was initially typed as a string, so the natural default `summarize.all_history=false` failed schema deserialization. The fix uses a concrete recursive JSON enum so boolean, numeric, string, null, array, and object defaults stay type-preserving without reintroducing a bare `serde_json::Value` input schema.

## Research Applied

- OpenTelemetry error guidance says error classification should use a predictable, low-cardinality `error.type`, and successful operations should not set it. The implementation records status one-hot slots and low-cardinality error types such as `AGENT_COST_FLEET_ROLLUP_UNAVAILABLE`, while leaving successful calls without error classification. Source: https://opentelemetry.io/docs/specs/semconv/general/recording-errors/ and https://opentelemetry.io/docs/specs/semconv/registry/attributes/error/
- OpenTelemetry feature-flag guidance records evaluations with bounded attributes and avoids carrying large/private result values. The implementation records size-bounded steering/policy rows, parameter shape hashes rather than raw parameter values, and concrete typed promotion values. Source: https://opentelemetry.io/docs/specs/semconv/feature-flags/feature-flags-events/
- Progressive-delivery guidance favors reversible flags, monitored rollout, and rollback. The implementation uses `shadow -> gates -> promoted` ledger rows plus an explicit `rolled_back` row, all anchored in Calyx. Sources: https://launchdarkly.com/blog/what-are-feature-flags/ and https://www.getunleash.io/blog/progressive-delivery-with-feature-flags

## Source Of Truth

- Runtime daemon: OS process table and socket table.
- MCP schema: initialized MCP `tools/list` from `http://127.0.0.1:7700/mcp`.
- Durable usage corpus: `C:\Users\hotra\AppData\Local\synapse\fsv-1691-calyx-db`, `CF_KV` keys under `mcp-usage/v1/`.
- Calyx measurement: `syn-mcp-usage-v1`, panel version `1691001`.
- Grounding anchors: physical Calyx Anchors CF rows for the exact `CF_KV` source keys.
- Operational aggregate: `telemetry operation=status` `tool_usage` and Prometheus samples.

## Structural Checks

No automated tests or FSV harnesses were added or run.

- `cargo fmt --all --check` passed.
- `cargo check -p synapse-mcp` passed.
- `cargo clippy --workspace --all-targets` passed.
- `cargo build --release -p synapse-mcp` passed; used only to run the real FSV daemon.

## MCP Preconditions

Process/socket SoT:

```text
pid=27972
ExecutablePath=C:\code\Synapse\target\release\synapse-mcp.exe
CommandLine="C:\code\Synapse\target\release\synapse-mcp.exe" --mode http --bind 127.0.0.1:7700 --db C:\Users\hotra\AppData\Local\synapse\fsv-1691-calyx-db --storage-backend calyx ...
Listen 127.0.0.1:7700 OwningProcess=27972
```

MCP health through the wired `mcp__synapse.health` tool:

```text
ok=true
pid=27972
storage.storage_backend=calyx
storage.db_path=C:\Users\hotra\AppData\Local\synapse\fsv-1691-calyx-db
tool_count=40
tool_surface_sha256=56660de70d065f31fadf233c43509cc69bf0c19e009a6750b95c5628f4ff6ce3
facade_contract.status=ok
public_tool_registry.status=ok
```

Initialized MCP `tools/list` schema readback:

```text
session=a111b6cd-4189-41da-bcb7-72a6f6202cca
tool_count=40
assist_operations=intent,detect,suggestion_tick,suggestion_list,suggestion_accept,guide,guide_policy,guide_promote,guide_rollback
storage_operations=inspect,summary,gc_once,anchors
promoted_value_def={"anyOf":[{"type":"null"},{"type":"boolean"},{"type":"integer"},{"type":"number"},{"type":"string"},{"items":{"$ref":"#/$defs/McpUsagePromotedValue"},"type":"array"},{"additionalProperties":{"$ref":"#/$defs/McpUsagePromotedValue"},"type":"object"}]}
```

Note: the current Codex process reported `SYNAPSE_CODEX_CURRENT_PROCESS_SCHEMA_STALE` in telemetry after the live daemon surface changed. The real MCP calls below still dispatched through `mcp__synapse`; follow-up issue #1740 tracks the stale visible metadata/handoff clarity.

## Baseline State

`storage operation=inspect` before the route exercise:

```text
storage_backend=calyx
calyx_vault.vault_id=01KXTTNJC25SV50EEKDDRKNA5F
calyx_vault.latest_seq=20
CF_KV live_row_count=5
CF_KV total_logical_bytes=2039
```

`assist operation=guide route_id=cost.summarize` before the route exercise:

```text
evidence_rows=0
success_rows=0
error_rows=0
latest_usage_row_key=null
active_policy=null
active_promotions=[]
steering=null
kernel_basis="syn-mcp-usage-v1 constellations over 0 usage rows with 0 grounded errors and 0 grounded successes"
```

## Edge Case Audit

Edge 1 - enable policy before evidence:

```text
Before: evidence_rows=0 active_policy=null steering=null
Trigger: assist guide_policy route_id=cost.summarize hint_id=cost_summarize_unbounded_scan enabled=true
Result: MCP_USAGE_POLICY_EVIDENCE_FLOOR_UNMET evidence_rows=0 floor=1
After: evidence_rows=0 active_policy=null steering=null
```

Edge 2 - structurally invalid route id:

```text
Before: evidence_rows=0 active_policy=null steering=null
Trigger: assist guide_policy route_id="cost summarize!" enabled=true
Result: MCP_USAGE_route_id_INVALID
After: evidence_rows=0 active_policy=null steering=null
```

Edge 3 - empty reason boundary:

```text
Before: evidence_rows=0 active_policy=null steering=null
Trigger: assist guide_policy reason=""
Result: MCP_USAGE_REASON_INVALID
After: evidence_rows=0 active_policy=null steering=null
```

Edge 4 - duplicate rollback after a real rollback:

```text
Before: active_promotions=[] promotion_id=cost.summarize-9337d89364eb9458 already has 003-rolled_back
Trigger: assist guide_rollback promotion_id=cost.summarize-9337d89364eb9458
Result: MCP_USAGE_PROMOTION_ALREADY_ROLLED_BACK rollback_row=mcp-usage/v1/promotion/cost.summarize-9337d89364eb9458/003-rolled_back
After: active_promotions=[] evidence_rows=3 error_rows=3 steering=null
```

## Happy Path

1. Seed known-bad evidence with real MCP trigger:

```text
Trigger: cost operation=summarize summarize.all_history=true, no spawn_id
Result: AGENT_COST_FLEET_ROLLUP_UNAVAILABLE, no steering yet
Guide after:
  evidence_rows=1
  error_rows=1
  latest_usage_row_key=mcp-usage/v1/call/1784385554667-27972-019f75aac8eb7a828880184a8cf1f115/00000000000000000011
  latest_usage_value_sha256=sha256:f43c661c252347449baf12675931c96931caf70a9c14cf0e0609626a34d105ec
Anchor read:
  panel_name=syn-mcp-usage-v1
  panel_version=1691001
  cx_id=b1172ffec11a1b017be68ad98e09eb86
  anchor kind=label:synapse:mcp_tool_call_outcome
  anchor value=error
  source_value_sha256=f43c661c252347449baf12675931c96931caf70a9c14cf0e0609626a34d105ec
```

2. Enable grounded steering:

```text
Trigger: assist guide_policy enabled=true
Policy row: mcp-usage/v1/policy/cost.summarize/cost_summarize_unbounded_scan/00000001784385654393
Storage exact_value_match=true
Constellation: panel=syn-mcp-usage-v1 cx_id=b46a9681cfb6acd547d70c285f0aae43
Anchor read:
  kind=label:synapse:mcp_steering_enabled
  value=true
  source_value_sha256=f2bead66ec011896da20cb914ffba4a656cfb74865bb63d9f3466ac7edde73de
Guide after:
  steering.hint_id=cost_summarize_unbounded_scan
  steering.suggested_next_tool=cost
  steering.suggested_next_operation=summarize
  steering.suggested_parameters=[summarize.spawn_id=<existing-agent-spawn-id>, summarize.all_history=false, summarize.include_per_turn=true only when per-turn rows are needed]
```

3. Repeat known-bad call with steering active:

```text
Trigger: cost operation=summarize summarize.all_history=true, no spawn_id
Result: AGENT_COST_FLEET_ROLLUP_UNAVAILABLE with data.steering
Guide after:
  evidence_rows=2
  error_rows=2
  latest_usage_row_key=mcp-usage/v1/call/1784385554667-27972-019f75aac8eb7a828880184a8cf1f115/00000000000000000017
  latest_usage_value_sha256=sha256:12052034786eab2c6c466926e4d7a5ba917473a9d009fa7c78e50d7bce81b956
Anchor read:
  panel_name=syn-mcp-usage-v1
  cx_id=856eacf3996ef466523a7a90c4dd5e93
  anchor kind=label:synapse:mcp_tool_call_outcome
  anchor value=error
```

4. Disable steering and prove the hint disappears:

```text
Trigger: assist guide_policy enabled=false
Policy row: mcp-usage/v1/policy/cost.summarize/cost_summarize_unbounded_scan/00000001784385687205
Anchor read:
  kind=label:synapse:mcp_steering_enabled
  value=false
  cx_id=5579c643984431fa1f093188719070f2
Trigger: cost operation=summarize summarize.all_history=true, no spawn_id
Result: AGENT_COST_FLEET_ROLLUP_UNAVAILABLE with no data.steering
Guide after:
  evidence_rows=3
  error_rows=3
  steering=null
  latest_usage_row_key=mcp-usage/v1/call/1784385554667-27972-019f75aac8eb7a828880184a8cf1f115/00000000000000000022
```

5. Promote and roll back a default parameter:

```text
Trigger: assist guide_promote route_id=cost.summarize parameter_path=summarize.all_history promoted_value=false
Promotion id: cost.summarize-9337d89364eb9458
Ledger rows:
  mcp-usage/v1/promotion/cost.summarize-9337d89364eb9458/000-shadow
  mcp-usage/v1/promotion/cost.summarize-9337d89364eb9458/001-gates
  mcp-usage/v1/promotion/cost.summarize-9337d89364eb9458/002-promoted
Exact row readbacks: all exact_value_match=true
Anchor reads:
  shadow: cx_id=80063dd60b91e131535927fe0b6cec3f value=shadow
  gates: cx_id=f215366d391ba0096cff9dc16356655f value=gates
  promoted: cx_id=3ccf7caa99f93117874938f630a2b46b value=promoted
Guide after promote:
  active_promotions=[{promotion_id=cost.summarize-9337d89364eb9458, parameter_path=summarize.all_history, promoted_value=false, state=promoted}]
  pattern.recommended_parameterizations includes summarize.all_history=false
Trigger: assist guide_rollback promotion_id=cost.summarize-9337d89364eb9458
Rollback row: mcp-usage/v1/promotion/cost.summarize-9337d89364eb9458/003-rolled_back
Rollback anchor:
  cx_id=e4b1a9128934edcc278425728d1c26ee
  kind=label:synapse:mcp_default_promotion_state
  value=rolled_back
Guide after rollback:
  active_promotions=[]
```

## Final State Evidence

Final `storage inspect` after the manual sequence:

```text
storage_backend=calyx
calyx_vault.latest_seq=133
calyx_vault.live_row_count=52
CF_KV live_row_count=42
CF_KV total_logical_bytes=39324
```

Final `telemetry operation=status` after the storage inspection itself was also measured:

```text
CF_KV row_count=43
calyx_constellation_measurements_total{panel="syn-mcp-usage-v1",source_cf="CF_KV",outcome="inserted"} 39
tool_usage cost.summarize: calls_total=3 error_total=3 latest_status=error
tool_usage assist.guide_promote: calls_total=1 ok_total=1
tool_usage assist.guide_rollback: calls_total=2 ok_total=1 error_total=1
tool_usage storage.anchors: calls_total=8 ok_total=8
```

## Follow-Up Issues Filed

- #1739 - Investigate Calyx daemon setup hang before bind during storage open.
- #1740 - Clarify Codex MCP tool-schema staleness after live daemon surface changes.

