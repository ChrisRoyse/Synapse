# Synapse 40-Tool Surface Migration

Updated: 2026-06-30

Parent issue: #1374
Task issue: #1394

## Current Surface

The default production MCP surface is facade-first:

- Public registry limit: 40 tools maximum.
- Public registry source of truth: `crates/synapse-mcp/src/server/tool_profiles.rs` `PUBLIC_TOOL_NAMES`.
- Normal-agent visible source of truth: `NORMAL_ALLOWED_EXACT`.
- Facade contract source of truth: `FACADE_TOOL_CONTRACTS`.
- Live profile row source of truth: `CF_SESSIONS mcp/tool-profile/v1/<session_id>`.

The current normal-agent profile exposes all 40 stable public facade names. Every scoped production HTTP profile advertises the same facade surface so strict/static MCP clients never need a schema refresh to gain a capability route. Profiles gate operation-level authority: debugger-backed `browser_debugger` operations require `profile operation=set profile=browser_debugger confirm_break_glass=true reason=<why debugger authority is required>`, and real-foreground actions use the audited `act operation=foreground` route. Raw implementation tools remain hidden for scoped HTTP profiles. Trusted unscoped stdio intentionally retains the full raw admin surface, and `SYNAPSE_DEBUG_TOOLS=1` explicitly enables the diagnostic implementation surface.

Default normal-agent tools:

`health`, `profile`, `session`, `subscribe`, `observe`, `find`, `read_text`, `screenshot`, `target`, `act`, `shell`, `process`, `browser_tabs`, `browser_nav`, `browser_dom`, `browser_form`, `browser_wait`, `browser_capture`, `browser_storage`, `browser_debugger`, `workspace`, `agent`, `task`, `approval`, `escalation`, `timeline`, `episode`, `routine`, `assist`, `reality`, `verification`, `storage`, `model`, `cost`, `hygiene`, `audit`, `replay`, `privacy`, `setup`, `telemetry`.

Public facade with profile-gated operations:

`browser_debugger`.

## Deprecation Policy

Normal agents use facade tools only. Old implementation names are not default public tools.

Every facade call uses a strict `operation` enum. Unknown operations fail closed with structured errors. There is no alias fallback from an old tool name to a new facade operation.

Mutating operations must name the physical readback source of truth: file path, RocksDB CF/key, process id, tab id, target id, event cursor, or profile row.

Raw browser debugger capability is explicit: switch to `browser_debugger` before using raw CDP/chrome.debugger operations.

Raw human OS foreground capability is explicit: acquire the needed foreground/control lease, switch to `break_glass`, give a non-empty reason, and read the post-action source of truth. Normal-agent routes must prefer target-scoped action through `act` and target/session state through `target`.

Removed status means removed from the normal public surface. It does not always mean implementation code was deleted. Maintenance, debugger, break-glass, or full-capability profiles may still retain implementation routes for controlled use.

## Facade Operation Index

| Facade | Operations |
| --- | --- |
| `profile` | `status`, `set` |
| `session` | `list` |
| `subscribe` | `events` |
| `observe` | `current` |
| `find` | `elements` |
| `read_text` | `text` |
| `screenshot` | `capture`, `gif` |
| `target` | `get`, `list`, `set`, `clear`, `claim`, `status`, `adopt`, `release` |
| `act` | `invoke`, `foreground` |
| `shell` | `run`, `start`, `status`, `cancel` |
| `process` | `list`, `launch`, `history` |
| `browser_tabs` | `list`, `select`, `new`, `close` |
| `browser_nav` | `navigate`, `reload`, `back`, `forward` |
| `browser_dom` | `content`, `locate`, `inspect`, `aria_snapshot` |
| `browser_form` | `set_value`, `fill` |
| `browser_wait` | `for_condition` |
| `browser_capture` | `screenshot`, `downloads` |
| `browser_storage` | `read`, `write` |
| `browser_debugger` | `evaluate`, `console_messages`, `pdf`, `file_upload`, `dialog`, `add_init_script`, `add_script_tag`, `add_style_tag`, `network`, `network_har`, `network_overrides`, `route`, `emulate`, `expose_binding`, `drag`, `drop` |
| `workspace` | `get`, `put`, `list`, `subscribe`, `exists`, `delete` |
| `agent` | `spawn`, `query`, `send`, `inbox`, `wait`, `broadcast`, `receipts`, `stats`, `template_put`, `template_get`, `template_list`, `template_delete`, `task_started`, `interrupt`, `kill`, `steer`, `pause`, `resume`, `respawn` |
| `task` | `create`, `get`, `update`, `claim`, `cancel`, `list`, `next`, `reconcile`, `dispatch_once` |
| `approval` | `request`, `list`, `decide`, `gate`, `ask_operator` |
| `escalation` | `config_get`, `config_set`, `list`, `ack` |
| `timeline` | `get`, `search`, `stats` |
| `episode` | `list`, `get` |
| `routine` | `mine`, `list`, `inspect`, `update`, `feedback`, `label`, `automate`, `armed_tick` |
| `assist` | `intent`, `detect`, `suggestion_tick`, `suggestion_list`, `suggestion_accept` |
| `reality` | `baseline`, `delta`, `audit` |
| `verification` | `inbox`, `poll`, `audit`, `bind`, `sources` |
| `storage` | `inspect`, `summary`, `put_probe_rows`, `gc_once` |
| `model` | `list`, `status`, `probe`, `register`, `update`, `remove` |
| `cost` | `summarize`, `price_list`, `price_put`, `price_delete` |
| `hygiene` | `scan_text`, `scan_storage`, `flags`, `report` |
| `audit` | `command_query`, `lifecycle_events`, `lifecycle_exits`, `profile_intelligence`, `export_bundle` |
| `replay` | `record`, `demo_status`, `demo_start`, `demo_stop`, `artifact_inspect` |
| `privacy` | `pause`, `resume`, `exclusions`, `redact`, `purge` |
| `setup` | `status`, `doctor`, `repair` |
| `telemetry` | `status` |

## Common Workflow Examples

Daemon/profile check:

```text
tool: health
args: {}

tool: profile
args: { "operation": "status" }
```

Targeted action:

```text
tool: target
args: { "operation": "list", "title_contains": "GameEditor" }

tool: target
args: { "operation": "set", "target": { "kind": "window", "window_hwnd": 123456 } }

tool: act
args: { "operation": "invoke", "action": { "verb": "click", "element_id": "known-element" } }
```

Already-open Chrome tab workflow:

```text
tool: browser_tabs
args: { "operation": "list" }

tool: browser_tabs
args: { "operation": "new", "url": "https://example.com" }

tool: browser_nav
args: { "operation": "navigate", "url": "https://example.com/status" }

tool: browser_dom
args: { "operation": "locate", "selector": "main" }
```

Workspace state with separate readback:

```text
tool: workspace
args: { "operation": "put", "put": { "key": "issue/1394/example", "value": { "state": "written" } } }

tool: workspace
args: { "operation": "get", "get": { "key": "issue/1394/example" } }
```

Shell job with durable status:

```text
tool: shell
args: { "operation": "start", "start": { "command": "pwsh", "args": ["-NoProfile", "-Command", "Get-Date"] } }

tool: shell
args: { "operation": "status", "status": { "job_id": "<returned job id>" } }
```

## Old-Name Coverage Table

Historical source: `git show 8ca2667a^:crates/synapse-mcp/src/server/tool_profiles.rs` `NORMAL_ALLOWED_EXACT`, plus the historical `NORMAL_ALLOWED_PREFIXES` expansion for `agent_template_*` and `task_*`.

The historical audit saw 152 live visible tools. The historical exact-plus-prefix inventory below contains 153 names because prefix expansion records every dynamic route that the profile allowed. Every name below is either routed to a current facade, an explicit advanced profile route, or removed from the normal public surface.

| Old public name(s) | Current route | Status |
| --- | --- | --- |
| `health` | `health` | Kept as public facade. |
| `tool_profile_status`, `tool_profile_set` | `profile operation=status`, `profile operation=set` | Condensed. |
| `session_list`, `session_status` | `session operation=list` | Condensed readback. |
| `session_end` | Advanced maintenance/session lifecycle route | Removed from normal public surface. |
| `profile_list`, `profile_authoring_generate`, `profile_authoring_inspect`, `profile_authoring_list` | Advanced profile-maintenance route | Removed from normal public surface. |
| `subscribe` | `subscribe operation=events` | Kept as public facade. |
| `subscribe_cancel` | Client/session subscription lifecycle | Removed from normal public surface. |
| `observe` | `observe operation=current` | Kept as public facade. |
| `observe_delta` | `reality operation=delta` | Condensed into reality facade. |
| `find` | `find operation=elements` | Kept as public facade. |
| `read_text` | `read_text operation=text` | Kept as public facade. |
| `capture_screenshot` | `screenshot operation=capture` | Condensed. |
| `capture_gif` | `screenshot operation=gif` | Condensed. |
| `window_list`, `get_target`, `set_target`, `clear_target` | `target operation=list/get/set/clear` | Condensed. |
| `set_capture_target`, `set_perception_mode` | `target operation=set` plus per-call `observe`/`screenshot` parameters | Removed as normal public global mutators. |
| `target_claim`, `target_claim_status`, `target_claim_adopt`, `target_release` | `target operation=claim/status/adopt/release` | Condensed. |
| `target_act` | `act operation=invoke` | Condensed through action facade. |
| `act_foreground` | `act operation=foreground` | Condensed; explicit reason required; the whole keyed authority transaction remains supervised through exact profile/lease cleanup and final audit after caller cancellation, and the dedicated physical operator-panic epoch supersedes stored agent lease snapshots. |
| `control_lease_acquire`, `control_lease_handoff`, `control_lease_release`, `control_lease_status` | `act operation=foreground`, `target operation=claim/status/release`, or explicit `break_glass` profile | Removed from normal public surface as standalone lease tools. |
| `act_run_shell`, `act_run_shell_start`, `act_run_shell_status`, `act_run_shell_cancel` | `shell operation=run/start/status/cancel` | Condensed. |
| `act_launch` | `process operation=launch` | Condensed. |
| `act_spawn_agent` | `agent operation=spawn` | Condensed. |
| `browser_tabs`, `browser_adopt_active_tab`, `cdp_open_tab`, `cdp_close_tab` | `browser_tabs operation=list/select/new/close` | Condensed. |
| `cdp_navigate_tab` | `browser_nav operation=navigate/reload/back/forward` | Condensed. |
| `cdp_activate_tab` | `browser_tabs operation=select`; explicit foreground activation requires advanced routing | Removed as direct normal public route. |
| `cdp_target_info` | `browser_tabs operation=list` and `target operation=status` | Condensed. |
| `cdp_bridge_reload` | `setup operation=doctor/repair` or advanced browser maintenance profile | Removed from normal public surface. |
| `browser_content`, `browser_locate`, `browser_inspect`, `browser_aria_snapshot` | `browser_dom operation=content/locate/inspect/aria_snapshot` | Condensed. |
| `browser_frames`, `browser_scroll_into_view`, `browser_set_content` | `browser_dom`, `browser_wait`, or advanced browser-control profile route | Removed from normal public surface as direct implementation seams. |
| `browser_wait_for`, `browser_assert` | `browser_wait operation=for_condition` | Condensed. |
| `browser_batch` | Use ordered calls to `browser_tabs`, `browser_nav`, `browser_dom`, `browser_form`, `browser_wait`, `browser_capture` | Removed from normal public surface to avoid ambiguous multi-side-effect batches. |
| `browser_clock`, `browser_page_events` | `browser_wait operation=for_condition`, `audit operation=command_query`, or advanced browser-control profile route | Removed from normal public surface. |
| `browser_set_value`, `browser_fill_form` | `browser_form operation=set_value/fill` | Condensed. |
| `browser_screenshot` | `browser_capture operation=screenshot` | Condensed. |
| `browser_downloads` | `browser_capture operation=downloads` | Condensed. |
| `browser_storage`, `browser_cookies` | `browser_storage operation=read/write` | Condensed. |
| `browser_file_upload` | Switch to `browser_debugger`, then `browser_debugger operation=file_upload` | Profile-gated advanced route. |
| `workspace_get`, `workspace_put`, `workspace_list`, `workspace_subscribe` | `workspace operation=get/put/list/subscribe`; new `exists/delete` also live | Condensed. |
| `agent_query`, `agent_inbox`, `agent_wait`, `agent_send`, `agent_send_broadcast`, `agent_receipts`, `agent_stats` | `agent operation=query/inbox/wait/send/broadcast/receipts/stats` | Condensed. |
| `agent_interrupt`, `agent_kill`, `agent_pause`, `agent_resume`, `agent_respawn`, `agent_steer` | `agent operation=interrupt/kill/pause/resume/respawn/steer` | Condensed. |
| `agent_template_put`, `agent_template_get`, `agent_template_list`, `agent_template_delete` | `agent operation=template_put/template_get/template_list/template_delete` | Condensed. |
| `agent_ask_operator` | `approval operation=ask_operator` | Condensed. |
| `agent_cost` | `cost operation=summarize` | Condensed. |
| `agent_spawn_task_started` | `agent operation=task_started` | Condensed; spawned-agent readiness stays in the visible agent facade without adding a public tool. |
| `fleet_stop` | `agent operation=kill` scoped by explicit ids, or advanced maintenance route for fleet-wide stops | Removed from normal public surface as broad destructive control. |
| `task_create`, `task_get`, `task_update`, `task_claim`, `task_cancel`, `task_list`, `task_next`, `task_reconcile`, `task_dispatch_once` | `task operation=create/get/update/claim/cancel/list/next/reconcile/dispatch_once` | Condensed. |
| `approval_request`, `approval_list`, `approval_decide`, `approval_gate` | `approval operation=request/list/decide/gate` | Condensed. |
| `escalation_list`, `escalation_ack` | `escalation operation=list/ack` | Condensed. |
| `timeline_get`, `timeline_search`, `timeline_stats` | `timeline operation=get/search/stats` | Condensed. |
| `timeline_digest` | `timeline operation=search/stats` plus agent-side summary, or future timeline summary operation | Removed from normal public surface as standalone route. |
| `timeline_pause`, `timeline_resume`, `timeline_exclusions`, `timeline_redact`, `timeline_purge` | `privacy operation=pause/resume/exclusions/redact/purge` | Condensed into privacy facade. |
| `episode_list`, `episode_get` | `episode operation=list/get` | Condensed. |
| `episode_segment` | Advanced episode/mining route | Removed from normal public surface. |
| `routine_mine`, `routine_list`, `routine_inspect`, `routine_update`, `routine_feedback`, `routine_label_export` | `routine operation=mine/list/inspect/update/feedback/label` | Condensed. |
| `armed_routine_tick` | `routine operation=armed_tick` for controlled routine profile, not normal-agent scheduling | Removed from normal public surface. |
| `intent_current`, `intent_detect_tick` | `assist operation=intent/detect` | Condensed; scheduler tick is no longer a standalone normal tool. |
| `suggestion_tick`, `suggestion_list`, `suggestion_accept` | `assist operation=suggestion_tick/suggestion_list/suggestion_accept` | Condensed; tick is controlled by assist/routine routing. |
| `reality_baseline`, `reality_audit` | `reality operation=baseline/audit` | Condensed. |
| `verification_inbox`, `verification_poll`, `verification_audit`, `verification_bind`, `verification_sources` | `verification operation=inbox/poll/audit/bind/sources` | Condensed. |
| `storage_inspect` | `storage operation=inspect` | Condensed. |
| `storage_gc_once`, `storage_put_probe_rows` | `storage operation=gc_once/put_probe_rows` only under maintenance/debug policy | Removed from normal public surface as mutating maintenance/debug routes. |
| `local_model_list`, `local_model_probe` | `model operation=list/probe` and `model operation=status` | Condensed. |
| `local_model_register`, `local_model_update`, `local_model_remove` | `model operation=register/update/remove` under explicit maintenance policy | Removed from normal public surface as registry mutators. |
| `hygiene_flags`, `hygiene_report`, `hygiene_scan_storage`, `hygiene_scan_text` | `hygiene operation=flags/report/scan_storage/scan_text` | Condensed. |
| `audit_intelligence_query` | `audit operation=profile_intelligence` | Condensed and narrowed. |
| `demo_record_start`, `demo_record_stop` | `replay operation=demo_start/demo_stop`; status via `replay operation=demo_status` | Condensed and no longer default demo controls. |

## Edge Rules

Old tool missing from mapping: treat as documentation failure and update this file before closing a migration issue.

Removed tool still public in `normal_agent`: treat as profile regression. Fix `NORMAL_ALLOWED_EXACT` or the profile router before claiming the 40-tool surface is live.

Doc example uses hidden route from `normal_agent`: treat as a documentation regression. Examples must use current facade tools only.

Public count above 40: treat as release blocker for #1374. The source of truth is production-client `tools/list`, not a local constant alone.
