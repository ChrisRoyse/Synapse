# 16. API / Tools Reference

> Current production note, 2026-06-30: the default `normal_agent` MCP surface is now facade-first and capped under the 40-tool contract tracked by #1374. This older reference still documents implementation and advanced/profile-gated routes. For the old-to-new public migration table and deprecation policy, see [../../docs2/SYNAPSE_40_TOOL_SURFACE_MIGRATION.md](../../docs2/SYNAPSE_40_TOOL_SURFACE_MIGRATION.md).

**Source files covered:**

- `crates/synapse-mcp/src/server.rs` (tool-router composition / registration)
- `crates/synapse-mcp/src/server/m1_tools.rs` (perception + browser/CDP)
- `crates/synapse-mcp/src/server/m2_tools.rs` (foreground/background action primitives)
- `crates/synapse-mcp/src/server/m3_tools.rs` (reflex, profiles, timeline, hygiene, approvals, local models, storage)
- `crates/synapse-mcp/src/server/m4_tools.rs` (combo, shell, launch, spawn)
- `crates/synapse-mcp/src/server/background_router.rs` (`target_act`)
- `crates/synapse-mcp/src/server/agent_control.rs`, `agent_cost.rs`, `agent_stats.rs`, `agent_query.rs`, `agent_mailbox.rs`, `agent_templates.rs`, `agent_tasks.rs`
- `crates/synapse-mcp/src/server/session_tools.rs`, `lease_tools.rs`, `target_claims.rs`
- `crates/synapse-mcp/src/server/browser_assert.rs`, `browser_clock_events.rs`, `browser_dialog.rs`, `browser_dnd.rs`, `browser_emulate.rs`, `browser_field.rs`, `browser_files.rs`, `browser_frames.rs`, `browser_network.rs`, `browser_storage.rs`
- `crates/synapse-mcp/src/server/intent_tools.rs`, `plan_tools.rs`, `reality.rs`, `suggestions.rs`, `routine_feedback.rs`, `routine_labeling.rs`
- `crates/synapse-mcp/src/server/timeline_query.rs`, `timeline_digest.rs`, `data_cleaning.rs`
- `crates/synapse-mcp/src/server/workspace_blackboard.rs`, `tool_profiles.rs`, `notify_tools.rs`, `hygiene_report.rs`, `permission_gate.rs`, `escalation/mod.rs`
- `crates/synapse-mcp/src/server/everquest_*.rs` (17 files, opt-in domain pack)

> See [15_mcp_server_architecture.md](15_mcp_server_architecture.md) for transport, session lifecycle, and routing internals. Subsystem cross-refs are noted per domain (docs 05–14).

---

## 16.1 Registration, naming, and counts

Tools are registered through the **`rmcp`** crate's attribute macros. Each tool is a `pub async fn` on `SynapseService` annotated with `#[tool(description = "...")]` (sometimes `input_schema = ...` for hand-rolled schemas). Each source module groups its tools under a per-module `#[tool_router(router = <name>_tool_router, vis = "pub(super)")] impl SynapseService { ... }` block. Parameter structs are passed as `Parameters<XxxParams>` and derive `Deserialize` + `schemars::JsonSchema`; most use `#[serde(deny_unknown_fields)]`. Field-level defaults come from `#[serde(default = "...")]` and ranges/defaults are surfaced to the schema via `#[schemars(...)]`.

The full surface is assembled in `SynapseService::tool_router()` (`server.rs:603`) by summing every module router with `+`. Tool **names** are the function names verbatim (e.g. `fn observe` -> tool `observe`). Over MCP they appear to clients as `mcp__synapse__<name>`.

**Total tools documented: 207.**

- **Core surface (always registered): ~182 tools.**
- **EverQuest domain pack: 25 tools**, registered only when `SYNAPSE_ENABLE_EVERQUEST` is set (`server.rs:649`). rmcp builds the tool list once per service, so this is a startup opt-in.
- **Debug-only:** `storage_put_probe_rows` and `storage_pressure_sample` are removed from the default surface and remain callable only when `SYNAPSE_DEBUG_TOOLS` is set (`server.rs:671`).

Conventions seen throughout:
- Browser/CDP tools accept `cdp_target_id` + `window_hwnd` to address a session-owned tab, defaulting to the active session target; they are background-safe (no tab activation, no OS foreground, no human-foreground fallback).
- Action tools take `verify_delta` / `verify_timeout_ms` and a `backend` (auto/software/hardware) selector.
- Many tools persist to RocksDB column families (`CF_KV`, `CF_TIMELINE`, `CF_EPISODES`, `CF_ROUTINES`, `CF_AGENT_TRANSCRIPTS`, etc.) and return an exact row readback.

> Detail note: where a param table below says "(summarized)" the field list is abbreviated to key/required params; full field sets exist in the named struct. EverQuest tool params are summarized (opt-in pack).

---

## 16.2 Perception / M1 — `m1_tools.rs`

Source-of-truth observation, OCR, screenshots, window enumeration, and the CDP/Chrome-bridge browser surface. See [07_perception_subsystem.md](07_perception_subsystem.md), [06_accessibility_and_cdp_subsystem.md](06_accessibility_and_cdp_subsystem.md), [05_capture_subsystem.md](05_capture_subsystem.md).

| Tool | Description | Key params | Side effects |
|------|-------------|------------|--------------|
| `health` | Return server health | none (empty schema) | none |
| `observe` | Snapshot accessibility slots/entities for the target | `include: Vec<ObserveSlot>`, `depth?`, `max_elements?`, `element_offset?`, `subtree_root?`, `since_event_seq?`, `window_hwnd?` | read-only |
| `find` | Search visible a11y nodes & detected entities; flags suspected prompt injection | `query?`, `role?`, `name_substring?`, `automation_id?`, `scope?`, `limit?`, `in_window?`, `window_hwnd?` | read-only |
| `read_text` | OCR a region/element | `region?`, `element_id?`, `window_hwnd?`, `backend` (default Auto), `lang_hint?` | read-only |
| `capture_screenshot` | Write PNG/JPEG of target/region | `path` (req), `region?`, `window_hwnd?`, `overwrite=false` | writes file |
| `browser_screenshot` | Write PNG/JPEG page screenshot from a normal Chrome bridge tab | `path` (req), `scope` (Viewport), `clip?`, `element_id?`, `masks?`, `format?`, `quality?`, `omit_background=false`, `cdp_target_id?`, `window_hwnd?`, `overwrite=false` | writes file; queues `captureVisibleTab` calls to avoid Chrome capture quota; restores tab/scroll/masks; reports `required_foreground` when Chrome window focus is needed |
| `browser_pdf` | Write PDF from a normal Chrome bridge tab | `path` (req .pdf), `landscape=false`, `print_background=false`, `paper_width?`, `paper_height?`, `margin_*?`, `scale?`, `page_ranges?`, `prefer_css_page_size=false`, `cdp_target_id?`, `window_hwnd?`, `overwrite=false` | writes PDF via narrow `Page.printToPDF` bridge lane; reports byte count/hash and target readback |
| `browser_downloads` | List/wait/save/move normal Chrome downloads | `operation` (List), `window_hwnd?`, `download_id?`, `url_contains?`, `filename_contains?`, `mime_contains?`, `state?`, `since_unix_ms?`, `limit?`, `wait_timeout_ms?`, `path?`, `overwrite=false` | lists Chrome download rows/events; wait blocks for target state; save/move writes chosen path with bytes/SHA-256 |
| `hidden_desktop_pip_frame` | Read-only PiP frame of a session-owned hidden desktop window | `window_hwnd` (req), `path` (req), `watched_session_id?`, `region?`, `overwrite=false` | writes PNG; never forwards input |
| `set_capture_target` | Set active capture target | `target: CaptureTargetParam` (req), `min_update_interval_ms?`, `cursor_visible?`, `dirty_region_only?` | session state |
| `set_perception_mode` | Set active perception mode | `mode: String` (req) | session state |
| `set_target` | Bind session perception target (Window or CDP) | `target: SetTargetParam` (Window{hwnd} \| Cdp{hwnd,cdp_target_id}) | session state |
| `get_target` | Return active perception target | none | read-only |
| `clear_target` | Clear perception target (revert to global foreground) | none | session state |
| `window_list` | Enumerate top-level windows | `title_contains?`, `process_name_contains?`, `exclude_minimized=false` | read-only |
| `cdp_open_tab` | Open background Chromium tab, bind to session | `url` (req), `window_hwnd?` | opens tab; durable owner row |
| `cdp_close_tab` | Close a Synapse-created CDP tab | `cdp_target_id` (req) | closes tab |
| `cdp_target_info` | Read CDP target metadata | `window_hwnd?`, `cdp_target_id?` | read-only |
| `cdp_bridge_reload` | Reload the normal-Chrome bridge extension | `wait_timeout_ms?` (default 10000, cap 30000) | reloads extension worker |
| `browser_tabs` | List/select/new/close tabs | `operation` (default List), `window_hwnd?`, `cdp_target_id?`, `url?` | may open/close tab |
| `browser_adopt_active_tab` | Adopt foreground Chromium tab as session target | `window_hwnd?` | session target binding |
| `cdp_navigate_tab` | Navigate/reload/back/forward owned tab | `action` (req), `url?`, `window_hwnd?`, `cdp_target_id?`, `wait_timeout_ms?`, `ignore_cache?` | navigates tab |
| `cdp_activate_tab` | Activate (foreground) a tab | `window_hwnd?`, `cdp_target_id?`, `wait_timeout_ms?` | activates tab |
| `browser_evaluate` | Eval JS in owned tab | `expression` (req), `cdp_target_id?`, `window_hwnd?`, `element_id?`, `args?`, `await_promise=true`, `return_by_value=true` | runs JS |
| `browser_expose_binding` | Add/read/remove a `window` binding fn | `operation` (Add), `name` (req), `cdp_target_id?`, `window_hwnd?`, `execution_context_name?`, `since_seq?`, `max_calls=200` | injects/reads binding |
| `browser_file_upload` | Set/clear file inputs or intercept a pending file chooser in normal Chrome | `operation` (SetFiles), `files?`, one of `selector?`/`element_id?`/`active_element`, `since_seq?`, `limit=20`, target params | validates local files; uses `DOM.setFileInputFiles` and `Page.fileChooserOpened`; never opens an OS file picker |
| `browser_add_init_script` | Add/remove a Playwright-style init script | `operation` (Add), `source?`, `identifier?`, `world_name?`, `include_command_line_api=false`, `run_immediately=false`, `cdp_target_id?`, `window_hwnd?` | mutates page init |
| `browser_add_script_tag` | Inject `<script>` (url/content/path) | one of `url?`/`content?`/`path?`, `script_type?`, `cdp_target_id?`, `window_hwnd?` | DOM mutation |
| `browser_add_style_tag` | Inject `<style>`/`<link>` (url/content/path) | one of `url?`/`content?`/`path?`, `cdp_target_id?`, `window_hwnd?` | DOM mutation |
| `browser_wait_for` | Unified wait (#1348): `condition` selects text/load_state/url/selector/function/request/response, with the matching nested spec carrying that predicate's former-standalone-tool params | `condition` (req), one of `text`/`load_state`/`url`/`selector`/`function`/`request`/`response` spec objects | read-only (selector/function run JS) |
| `browser_content` | Get full HTML | `cdp_target_id?`, `window_hwnd?`, `max_bytes=2MiB` | read-only |
| `browser_set_content` | Replace main-frame HTML | `html` (req), `wait_timeout_ms?`, target params | DOM replacement |
| `browser_console_messages` | Read captured console entries (delta via `since_seq`) | `since_seq?`, `level?`, `source?`, `text_contains?`, `max_messages=200`, target params | arms console buffer |
| `browser_inspect` | Introspect one element | `element_id` (req), `max_html_bytes=256KiB`, target params | read-only |
| `browser_scroll_into_view` | Scroll element into view | `element_id` (req), target params | scrolls page |
| `browser_locate` | Resolve element ids via locator engine | `query` (req), `engine` (Css), locator filters, `limit=50` (summarized) | read-only |

## 16.3 Action / M2 — `m2_tools.rs`

Click/type/value/key/stroke/scroll/pad/clipboard primitives. Foreground tiers require the input lease; background tiers (CDP/UIA/PostMessage) do not. See [09_action_subsystem.md](09_action_subsystem.md) and §16.10.5 (leases). All take `verify_delta`/`verify_timeout_ms` (default 2000ms, 50–5000).

| Tool | Description | Key params | Side effects |
|------|-------------|------------|--------------|
| `act_click` | Click element/coordinate via UIA/native/CDP | `target` (req), `button=Left`, `clicks=1`, `modifiers`, `backend=Auto`, `use_invoke_pattern=true`, `auto_wait=false` | emits input/CDP click |
| `act_type` | Type text into focused element | `text` (req), `into_element?`, `dynamics=Natural`, `backend=Auto`, `press_enter_after=false`, `verify_delta=true` | emits keystrokes |
| `act_set_value` | Set element value (WM_SETTEXT or UIA ValuePattern; no fg fallback) | `element_id` (req), `text` (req), `verify_timeout_ms` | sets value |
| `act_set_field_text` | Clear+type+verify across web/UIA/native | `text` (req), `element_id?`, `locator?`, `auto_wait=false` | replaces field |
| `act_focus_window` | Lease-gated foreground activation | `hwnd?` \| `title_regex?` \| `pid?`, `stable_ms=75` | acquires lease; activates window |
| `act_press` | Press keys (CDP/PostMessage software or fg hardware) | `keys` (req), `hold_ms=33`, `backend=Auto`, `window_hwnd?`, `cdp_target_id?`, `auto_wait` | emits keys |
| `act_keymap` | Press a keymap alias | `alias` (req), `hold_ms=33`, `backend=Auto`, `window_hwnd?`, `cdp_target_id?` | emits keys |
| `act_stroke` | Move/aim/drag along point/path | `duration_or_speed` (req), `path?`/`target?`/`from?`/`to?`, `button?`, `motion_model=Path`, `backend=Auto` | emits mouse motion |
| `act_scroll` | Scroll at pointer/point | `dy=0`, `dx=0`, `at?`, `target?`, `smooth=false` | emits scroll |
| `act_pad` | Apply virtual gamepad report | `report` (req), `pad_id=0`, `controller=X360`, `backend=Vigem`, `hold_ms?` | virtual gamepad |
| `act_clipboard` | Read/write/clear session virtual clipboard | `verb` (req), `text?`, `format=Text` | virtual clipboard (no OS clipboard by default) |
| `action_diagnostic_rate_limit_override` | FSV diag: force rate limiter empty | `confirm`, `ttl_ms=5000` | mutates rate limiter (test) |
| `action_diagnostic_queue_full_setup` | FSV diag: saturate action queue | `confirm`, `blocker_duration_ms=5000` | blocks action queue (test) |
| `release_all` | Release all held input state | none | resets input |

## 16.4 Action / M4 — `m4_tools.rs`

Higher-level action and process/agent launch.

| Tool | Description | Key params | Side effects |
|------|-------------|------------|--------------|
| `act_combo` | Timed one-shot key sequence | `steps: Vec<ActComboStep>` (1–256, req), `backend=Auto`, `idempotency_key?` | emits keys |
| `act_run_shell` | Run allowlisted shell command (inline/durable, SSH-aware) | `command` (req), `args`, `working_dir?`, `env`, `timeout_ms=30000`, `execution_mode=Auto`, `durable_timeout_ms?`, `idempotency_key?` | spawns process |
| `act_run_shell_start` | Start durable background shell job | `command` (req), `args`, `working_dir?`, `env`, `timeout_ms?`, `job_id?` | spawns durable job |
| `act_run_shell_status` | Read durable job status/logs | `job_id` (req), `tail_bytes=8192` | read-only |
| `act_run_shell_cancel` | Cancel only the recorded durable job (+remote SSH cleanup when that job owns it) | `job_id` (req) | terminates job-owned process tree with readback |
| `act_launch` | Launch allowlisted process (optional hidden desktop) | `target` (req), `args`, `working_dir?`, `env`, `wait_for_window_title_regex?`, `timeout_ms=30000`, `cdp_debug?`, `desktop?`, `windows_console_window_state?` | spawns process |
| `act_spawn_agent` | Spawn durable child agent (Claude/Codex/local) | `template_id?`, `cli?`/`kind?`, `model?`, `model_ref?`, `prompt?`, `target?`, `working_dir?`, `mcp_url=127.0.0.1:7700/mcp`, `wait_timeout_ms=30000`, `require_approval_gate=true` | spawns agent process; waits for readiness |
| `agent_spawn_task_started` | Legacy implementation route for spawned-agent cooperative readiness; normal public profile uses `agent operation=task_started` | `spawn_id` (req) | writes task-started.json |

## 16.5 Reflex / M3 (reflexes, subscriptions, replay, audio) — `m3_tools.rs`

See [10_reflex_subsystem.md](10_reflex_subsystem.md), [08_audio_subsystem.md](08_audio_subsystem.md).

| Tool | Description | Key params | Side effects |
|------|-------------|------------|--------------|
| `subscribe` | Subscribe to filtered event notifications | `kinds`, `filter?`, `snapshot_first=false`, `buffer_size=4096` | opens subscription |
| `subscribe_cancel` | Cancel a subscription | `subscription_id` (req) | closes subscription |
| `reflex_register` | Register a reflex (event->action / aim) | `kind` (req), `when?`, `then?`, `debounce_ms?`, aim fields (`target?`,`axis?`,`gain?`,…) | registers reflex |
| `reflex_cancel` | Cancel a reflex | `reflex_id` (req) | cancels reflex |
| `reflex_list` | List reflexes | `include_expired?` | read-only |
| `reflex_history` | Persisted reflex audit history | `reflex_id?`, `limit` | read-only |
| `audio_tail` | Latest loopback audio tail (PCM s16le) | `seconds?` | read-only |
| `audio_transcribe` | Whisper-tiny transcription of tail | `seconds?`, `language?` | runs ASR |

## 16.6 Profiles, registry & authoring — `m3_tools.rs`

See [11_profiles_subsystem.md](11_profiles_subsystem.md).

| Tool | Description | Key params | Side effects |
|------|-------------|------------|--------------|
| `profile_list` | List loaded profiles | `include_inactive=true` | read-only |
| `profile_activate` | Activate a profile by id | `profile_id` (req) | activates profile |
| `profile_authoring_generate` | Generate candidate patch from replay+audit | `profile_id` (req), `replay_path?`, `max_audit_rows=500`, `max_replay_rows=500`, `candidate_id?` | writes candidate |
| `routine_automate` | Promote mined routine -> authoring candidate | `routine_id` (req), `profile_id?`, `candidate_id?`, `store_plan=true` | writes candidate + plan + status |
| `profile_authoring_list` | List authoring candidates | `profile_id?`, `state?`, `limit=100` | read-only |
| `profile_authoring_inspect` | Inspect one candidate | `candidate_id` (req) | read-only |
| `profile_authoring_decide` | Accept/reject a candidate | `candidate_id` (req), `decision` (req), `operator_note?`, `reason?` | mutates candidate state |
| `profile_authoring_export` | Export candidate bundle | `candidate_id` (req), `output_path` (req) | writes file |
| `profile_quality_refresh` | Refresh profile quality scoring | `profile_id` (req), `max_audit_rows?` | writes scoring rows |
| `profile_registry_query` | Query registry rows (search/inspect/report) | `view` (req), filters (summarized) | read-only |
| `profile_registry_install` | Install/update a registry package | `source_id` (req), `manifest_path` (req) | writes registry rows |
| `profile_registry_disable` | Disable/remove an installed row | `profile_id` (req), `state` (req) | mutates registry |
| `profile_registry_export` | Export registry rows to JSON | `output_path` (req), `row_kind?`, `limit?` | writes file |
| `profile_registry_import` | Import registry bundle | `bundle_path` (req) | writes registry rows |
| `profile_registry_rollback` | Rollback to prior trusted package | `profile_id` (req), `target_package_id?`, `target_package_version?` | mutates registry |
| `audit_intelligence_query` | Summarize profile-linked audit outcomes | `profile_id` (req), `max_rows?` | read-only |
| `audit_export_bundle` | Export redacted audit bundle (consent) | `profile_id` (req), `output_path` (req), `redaction_policy?`, `consent?`, `max_rows`, `max_row_bytes` | writes file |
| `replay_record` | Record observations/events to replay JSONL | `target` (req), `format` (req), `duration_ms?` | writes file |
| `demo_record_start` | Arm UIA demonstration recording | `profile_id` (req), `duration_ms=600000`, `path?`, `label?` | writes DemoMarker timeline rows |
| `demo_record_stop` | Stop demo recording, export replay JSONL | `demo_id?` | writes replay file |

## 16.7 Hygiene & local models — `m3_tools.rs`

Prompt-injection scoring + operator-supplied OpenAI-compatible model registry. See [13_models_subsystem.md](13_models_subsystem.md).

| Tool | Description | Key params | Side effects |
|------|-------------|------------|--------------|
| `hygiene_scan_text` | Score one text blob for prompt injection | `text` (req), `min_score=50`, `persist=false`, `source_cf?`, `source_key_hex?`, `source_field?` | optional CF_KV flag rows |
| `hygiene_scan_storage` | Batch-scan CF_OBSERVATIONS/CF_TIMELINE | `source_cfs?`, `limit_rows=1000`, `flag_limit=200`, `min_score=50`, `cursor?` | writes flag rows |
| `hygiene_flags` | Query persisted hygiene flag rows | `source_cf?`, `source_key_hex?`, `min_score=0`, `limit=100`, `cursor?` | read-only |
| `local_model_register` | Register local model endpoint (after tool-call probe) | `name`,`base_url`,`model_id` (req), `api_shape?`, `runtime_preset?`, `context_length?`, `max_tools?`, `notes?` | writes CF_KV (no key value stored) |
| `local_model_list` | List local model rows | `name?`, `include_disabled=true`, `limit` | read-only |
| `local_model_update` | Update a model row (re-probes on endpoint change) | `name` (req), `new_name?`, `base_url?`, `model_id?`, … | writes CF_KV |
| `local_model_remove` | Remove a model row | `name` (req) | deletes CF_KV row |
| `local_model_probe` | Re-probe a model with forced tool-call | `name` (req), `timeout_ms?` | writes health metadata |
| `hygiene_report` | Hygiene flags with downstream-impact view | `source_cf?`, `source_key_hex?`, `min_score?`, `time_range?`, `limit?`, `cursor?` (`hygiene_report.rs`) | read-only |
| `timeline_redact` | Mask hygiene flags in physical source rows | `flag_ids?`, `source_cf?`, `source_key_hex?`, `min_score?`, `dry_run?`, `invalidate?`, `marker?` (`data_cleaning.rs`) | mutates source rows |

## 16.8 Storage — `m3_tools.rs`

| Tool | Description | Key params | Side effects |
|------|-------------|------------|--------------|
| `storage_inspect` | Inspect CF sizes/row counts/pressure | (minimal/none) | read-only |
| `storage_gc_once` | One row-cap GC pass (diagnostic) | `cf_name`, `soft_cap_rows`, `hard_cap_rows` (req) | deletes rows |
| `storage_put_probe_rows` | Write synthetic probe rows **(debug-gated)** | `cf_name`, `rows`, `value_bytes` (req) | writes rows |
| `storage_pressure_sample` | Apply synthetic free-byte sample **(debug-gated)** | `free_bytes` (req) | mutates pressure state |

## 16.9 Approvals & escalation — `m3_tools.rs`, `permission_gate.rs`, `escalation/mod.rs`

| Tool | Description | Key params | Side effects |
|------|-------------|------------|--------------|
| `approval_request` | Enqueue durable human decision request | `kind` (req), `title` (req), `body` (req), `payload_json?`, `dedupe_key?`, `destructive?`, `notify?`, `suppress_popup?`, `timeout_ms?`, `timeout_decision?` | writes CF_KV |
| `approval_list` | List approval/suggestion queue rows | `statuses?`, `kinds?`, `include_terminal=false`, `limit?`, `cursor?` | materializes expired rows; audit row |
| `approval_decide` | Resolve item accept/decline/snooze | `approval_id` (req), `decision` (req), `note?`, `snooze_ms?`, `edited_args?`, `response_text?` | writes CF_KV + audit |
| `approval_gate` | Permission-prompt tool for spawned agents (#927) | `tool_name?`, `input?`, `tool_use_id?`, `spawn_id?` | gates tool call |
| `agent_ask_operator` | Respond-only needs-input tool for spawned/local agents (#1028) | `question` (req), `context?`, `timeout_ms?`, `spawn_id?`, `notify=true`, `suppress_popup=false` | writes `agent_question` approval row and blocks until response/decline/timeout |
| `escalation_config_set` | Configure AFK escalation engine | `webhooks?`, `min_tier1_severity?`, `ack_window_ms?`, `critical_ack_window_ms?`, `quiet_hours?` | writes policy (secrets stored, never echoed) |
| `escalation_config_get` | Read current escalation policy | none | read-only (secrets redacted) |
| `escalation_list` | List durable escalations + ladder state | `anchor?`, `status?`, `severity?`, `limit?`, `after_id?` | read-only |
| `escalation_ack` | Acknowledge an escalation (stop ladder) | `escalation_id` (req), `note?` | writes ack to audit log |

Public <=40-tool clients use the `approval` facade for Claude permission prompts; it accepts Claude's direct `tool_name`/`input` payload and delegates to hidden `approval_gate`.

## 16.10 Agents & orchestration

### 16.10.1 Lifecycle control — `agent_control.rs`

| Tool | Description | Params | Side effects |
|------|-------------|--------|--------------|
| `agent_interrupt` | Graceful interrupt via ranked clean channels | `session_id` (req) | sends interrupt; no kill |
| `agent_kill` | Force-terminate one resolved Synapse-spawned agent/session tree | `session_id` (req), `grace_ms=` (0–120000), graceful-first bool (default true) | terminates recorded agent-owned tree with readback |
| `fleet_stop` | Stop/interrupt only confirmed Synapse-managed agents matching the filter | `mode` (req, `kill`/`interrupt`), `confirm` (req, `STOP-FLEET`), `agent_kinds?` | terminates/interrupts recorded managed agents |
| `agent_steer` | Inject steering instruction (turn/steer or mailbox) | `session_id` (req), `instruction` (req, ≤16000), receipt bool (default true) | steers agent; mailbox row |
| `agent_pause` | Suspend agent process tree (NtSuspendProcess) | `session_id` (req) | suspends threads |
| `agent_resume` | Resume paused agent tree | `session_id` (req) | resumes threads |
| `agent_respawn` | Stop the resolved prior Synapse agent if live, then relaunch from its spawn metadata | `session_id` (req), `prompt` (req), continuity-packet bool (default true) | terminates recorded agent-owned tree + spawns agent |

### 16.10.2 Cost & stats — `agent_cost.rs`, `agent_stats.rs`

| Tool | Description | Params | Side effects |
|------|-------------|--------|--------------|
| `agent_cost_price_put` | Upsert model price (USD per Mtok) into local cost table | model + input/output rates (req) | writes CF_KV |
| `agent_cost_price_list` | List all price rows by model id | (none) | read-only |
| `agent_cost_price_delete` | Delete a model price row | model (req) | deletes row |
| `agent_cost` | Roll up token usage/cost from transcripts | `spawn_id?`, `since_ns?`, `until_ns?`, per-turn series bool | read-only (scans CF_AGENT_TRANSCRIPTS) |
| `agent_stats` | Metrics rollup over agent event journal | `since_ns?`, `until_ns?`, `spawn_id?` (summarized) | read-only |

### 16.10.3 Query & mailbox — `agent_query.rs`, `agent_mailbox.rs`

| Tool | Description | Params | Side effects |
|------|-------------|--------|--------------|
| `agent_query` | What is this agent doing & why (+optional cooperative deep answer) | `session_id` (req), `max_events=` (1–200), `lookback_ms`, `deep?` | read-only (+optional mailbox send) |
| `agent_send` | Send durable JSON message to a live MCP peer | `to_session` (req, id/`orchestrator`/successor), message, kind?, request_receipt?, ttl | writes CF_KV row |
| `agent_inbox` | Read this session's mailbox (drains by default) | `drain=true`, `kind?`, `limit` | drains/peeks CF_KV |
| `agent_wait` | Wait up to timeout for a mailbox message | `timeout_ms` (req, hard-bounded), `kind?`, drain | drains CF_KV |
| `agent_send_broadcast` | Broadcast to many sessions (all/kinds/explicit) | `to` (req), message, kind?, request_receipt?, exclude_self | one CF_KV row per recipient |
| `agent_receipts` | Read read-receipt box | `drain=true`, `from_session?`, `limit` | drains/peeks CF_KV |

### 16.10.4 Templates & tasks — `agent_templates.rs`, `agent_tasks.rs`

| Tool | Description | Params | Side effects |
|------|-------------|--------|--------------|
| `agent_template_put` | Create/edit spawn template | `template_id`, name/description, `model`, directory, `prompt` (req) | writes CF_KV |
| `agent_template_get` | Read one template | `template_id` (req) | read-only |
| `agent_template_list` | List templates by id | (filter params) | read-only |
| `agent_template_delete` | Delete a template | `template_id` (req) | deletes row |
| `agent operation=task_started` | Public facade route for spawned-agent cooperative readiness | `task_started.spawn_id` (req) | writes and reads back task-started.json |
| `task_create` | Create durable fleet task | title/description/acceptance, `priority` 1–5, `template_id` (+`template_params`) | writes task (FIFO order) |
| `task_get` | Read one task + attempt history | `task_id` (req) | read-only |
| `task_update` | Move state / edit fields (state machine validated) | `task_id` (req), state?, priority?, title?, … | mutates task; settles attempt |
| `task_claim` | Claim a todo task -> in_progress | `task_id`, `session_id` (req) | mutates task; appends attempt |
| `task_cancel` | Cancel task (terminal) | `task_id` (req), reason? | mutates task; fails attempt |
| `task_list` | List tasks (filtered, dispatch order); lazy reconcile | `state?`, filters | reconciles orphans |
| `task_next` | Preview dispatcher's next pick (no spawn) | `concurrency_cap?` | reconciles orphans |
| `task_reconcile` | Reconcile queue vs live sessions (crash-safe) | (minimal) | settles/flags attempts |
| `task_dispatch_once` | Atomically reconcile+select+spawn next task | `concurrency_cap?` (summarized) | spawns agent; binds attempt |

### 16.10.5 Sessions, leases & target claims — `session_tools.rs`, `lease_tools.rs`, `target_claims.rs`

| Tool | Description | Params | Side effects |
|------|-------------|--------|--------------|
| `session_list` | Cross-session read model (compact/full views) | `view?`, `include_*?`, `include_closed?`, `live_only?`, `cursor?`, `limit?` | read-only |
| `session_status` | One session row joined with foreground/lease/claims | `session_id` (req) | read-only |
| `session_end` | End/cleanup the caller's session or a fail-closed eligible stale/dead resource owner | `session_id` (req) (summarized) | terminates recorded session-owned resources; cleans rows |
| `control_lease_acquire` | Acquire/renew process-global input lease | (lease params; refuse-not-block) | mutates lease |
| `control_lease_release` | Release lease held by this session | none (empty schema) | mutates lease |
| `control_lease_handoff` | Atomically hand off lease to a named live peer | recipient session id (req) | transfers lease |
| `control_lease_status` | Read lease state (holder/age/TTL) | none (empty schema) | read-only |
| `target_claim` | Advisory ownership lease on a target | active target or explicit window/CDP target | writes claim; co-owned guard |
| `target_claim_adopt` | Recover claim from older same-agent session | `owner_session_id` (req) (summarized) | terminates old session; transfers claim |
| `target_release` | Release this session's claim | active or explicit target | releases claim |
| `target_claim_status` | Read live claims | `target?` | read-only |

## 16.11 Capability-preserving router — `background_router.rs`

| Tool | Description | Params | Side effects |
|------|-------------|--------|--------------|
| `target_act` | One high-level computer-use verb routed to the correct session-targeted primitive (background/foreground-equivalent, never implicit human-foreground fallback). Verbs: `read`, `screenshot`, `navigate`, `set_field`, `insert_text`, `append_text`, `set_selection`, `click`, `tap`, `dispatch_event`, `clear`, `focus`, `blur`, `select_text`, `check`/`uncheck`, `type`, `key`, `press`, `select`, `submit`, `save`, `cleanup_notepad_tabs`, `run_shell`, `focus_window`. | `verb` (req) + verb-specific: `url?`, `path?`, `element_id?`, `selector?`, `text?`, `key?`, `keys?`, `selection_start?`/`selection_end?`, `role?`, `name?`, `automation_id?`, `value?`, `option?`/`option_label?`/`option_index?`/`options?`, `event_type?`/`event_init?`, coords/clickCount, … (summarized — see `TargetActParams`) | inherits delegated primitive's effects; `ok=false` + `status=verify_needed/refused/error` on failure (no optimistic success) |

## 16.12 Browser / CDP extensions — `browser_*.rs`

All target a session-owned tab via `cdp_target_id?` + `window_hwnd?` (default active session target); background-safe. See [06_accessibility_and_cdp_subsystem.md](06_accessibility_and_cdp_subsystem.md).

| Tool | File | Description | Key params |
|------|------|-------------|-----------|
| `browser_aria_snapshot` | browser_assert.rs | Playwright-style ARIA snapshot | `root_element_id?`, `max_nodes?`, `max_depth?` |
| `browser_assert` | browser_assert.rs | Assert locator with bounded retry (visible/text/value/checked/enabled/attribute/count) | `locator` (req), `matcher` (req), expected_*, `timeout_ms=5000`, `interval_ms=100`, `negate?` |
| `browser_clock` | browser_clock_events.rs | Fake page clock (install/set/fast-forward/status) | `operation` (Status), `time_unix_ms?`, `delta_ms?` |
| `browser_page_events` | browser_clock_events.rs | Arm/read page lifecycle, popup, worker events | `since_seq?`, `limit=100`, `event_kind?`, `worker_type?` |
| `browser_handle_dialog` | browser_dialog.rs | Read/accept/dismiss JS dialogs via raw CDP or normal Chrome bridge | `operation` (Status), `default_policy?`, `prompt_text?`, `since_seq?`, `limit=20` |
| `browser_drag` | browser_dnd.rs | Drag element->element (CDP mouse default) | `source_selector`, `target_selector` (req), `mode?`, `steps=12`, `duration_ms=350`, `auto_wait=true` |
| `browser_drop` | browser_dnd.rs | HTML5 DragEvent drop (mode=mouse optional) | same `BrowserDndParams` (default mode Html5) |
| `browser_emulate` | browser_emulate.rs | Set/reset emulation overrides (viewport/device/geolocation/locale/media/network) in one call; subsumes the former single-domain browser_resize/device/geolocation/locale/media tools (#1348) | `operation` (Set), `domains` (req), `viewport?`/`device?`/`geolocation?`/`locale?`/`media?`/`network?` |
| `browser_set_value` | browser_field.rs | Replace field text via safe Chrome bridge | `text` (req), `selector?`, `active_element=false` |
| `browser_fill_form` | browser_field.rs | Fill multiple fields in one ordered call | `fields` (req, 1–200), `continue_on_error=false`, `wait_timeout_ms=5000` |
| `browser_frames` | browser_frames.rs | Enumerate composed frame tree | target params only |
| `browser_network` | browser_network.rs | Unified captured-network read (#1348): `mode` selects requests (filtered list), request (one by id w/ body), or websockets (lifecycle/frames) | `mode` (req), one of `requests`/`request`/`websockets` spec objects |
| `browser_network_har` | browser_network.rs | Record/replay/clear HAR 1.2 | `operation` (Record), `path?`, filters, `include_bodies=true`, `missing_policy=Passthrough` |
| `browser_network_overrides` | browser_network.rs | Set/get/clear extra headers + UA override (raw CDP only) | `operation` (Set), `headers` (req), `user_agent?` |
| `browser_route` | browser_network.rs | Add/list/remove/clear Fetch route rules | `operation` (AddFulfill), `route_id?`, `url?`, `match_kind` (Glob), `status=200`, `headers`, `body?`/`body_base64?`, `error_reason?`, continue_* |
| `browser_cookies` | browser_storage.rs | Get/set/clear cookies via chrome.cookies | (get/set/clear verb + cookie fields) |
| `browser_storage` | browser_storage.rs | Get/set/clear local/sessionStorage; save/load storageState | (verb + storage/state fields) |

## 16.13 Intent, plans & routines — `intent_tools.rs`, `plan_tools.rs`, `suggestions.rs`, `routine_*.rs`, `m3_tools.rs`

See [10_reflex_subsystem.md](10_reflex_subsystem.md), [11_profiles_subsystem.md](11_profiles_subsystem.md).

| Tool | File | Description | Key params |
|------|------|-------------|-----------|
| `intent_current` | intent_tools.rs | Rank routines the operator appears to be running now | `profile_id?`, `lookback_hours?`, `min_confidence?`, `max_candidates?`, `include_agent_activity?` |
| `intent_detect_tick` | intent_tools.rs | Force one intent-detection pass; publish transitions | `now_ts_ns?`, `min_confidence?`, `lookback_hours?` |
| `routine_compile_plan` | plan_tools.rs | Compile mined routine into executable setup plan | `routine_id` (req), `store?` |
| `plan_get` | plan_tools.rs | Read stored setup plan for a routine | `routine_id` (req) |
| `episode_segment` | m3_tools.rs | Segment timeline into episodes (CF_EPISODES) | `start_ts_ns?`, `end_ts_ns?`, `include_agent_activity=false`, `dry_run=false` |
| `episode_list` | m3_tools.rs | List episodes overlapping a range | `start_ts_ns?`, `end_ts_ns?`, `apps?`, `actor?`, `min_duration_ms?`, `limit`, `cursor?` |
| `episode_get` | m3_tools.rs | Fetch one episode + evidence refs | `episode_id` (req), `start_ts_ns?`, `refs_limit=500`, `refs_cursor?` |
| `routine_mine` | m3_tools.rs | Mine recurring routines into CF_ROUTINES | `start_ts_ns?`, `end_ts_ns?`, `min_support_days?`, `max_pattern_len?`, `include_agent_activity=false`, `dry_run=false` |
| `routine_list` | m3_tools.rs | List mined routines + lifecycle + taint | `lifecycle?`, `min_confidence?`, `app?`, `granularity?`, `include_unmined?`, `limit` |
| `routine_inspect` | m3_tools.rs | Fetch one routine (full record/state/taint/armed) | `routine_id` (req) |
| `routine_update` | m3_tools.rs | Lifecycle/arming mutation (confirm/disable/enable/archive/rename/arm/disarm) | `routine_id` (req), `action` (req), `label?`, `note?`, `arm_schedule?`, `arm_intent?`, `failure_threshold?` |
| `routine_label_export` | routine_labeling.rs | Prompt-ready naming bundle for one routine | `routine_id` (req), `max_samples?` |
| `routine_feedback` | routine_feedback.rs | Record how a routine suggestion resolved | `routine_id` (req), `outcome` (req), `now_ts_ns?` |
| `suggestion_tick` | suggestions.rs | Run one suggestion-engine pass (#858) | `now_ts_ns?`, `dry_run?` |
| `suggestion_list` | suggestions.rs | List surfaced suggestions | `status?`, `routine_id?` |
| `suggestion_accept` | suggestions.rs | Accept a suggestion + run its setup plan | `suggestion_id` (req), `dry_run?`, `browser_window_hwnd?`, `launch_timeout_ms?` |
| `armed_routine_tick` | suggestions.rs | Run one armed-routine pass (#862) | `routine_id?`, `trigger_mode?`, `dry_run?`, `browser_window_hwnd?`, `launch_timeout_ms?` |

## 16.14 Reality model — `reality.rs`

See [07_perception_subsystem.md](07_perception_subsystem.md).

| Tool | Description | Key params | Side effects |
|------|-------------|------------|--------------|
| `reality_baseline` | Capture/read delta-first reality baseline | `profile_id?`, `epoch_id?`, `force_new_epoch?`, `include?`, `depth?`, `max_elements?` | writes CF_KV reality rows |
| `observe_delta` | Observe reality, persist ordered changes, return deltas since cursor | `profile_id?`, `since_epoch?`, `since_seq?`, `include?`, `depth?`, `max_elements?`, `max_deltas?` | writes delta rows |
| `reality_audit` | Audit delta-guided assumption vs fresh read | `profile_id?`, `epoch_id?`, `assumption_hash?`, `include?`, `depth?`, `max_elements?` | writes drift findings |

## 16.15 Timeline — `timeline_query.rs`, `timeline_digest.rs`, `m3_tools.rs`

CF_TIMELINE is the operator-activity feed.

| Tool | File | Description | Key params |
|------|------|-------------|-----------|
| `timeline_get` | timeline_query.rs | Raw timeline rows in ascending time order (stable cursor) | `start_ts_ns`, `end_ts_ns`, `limit?`, `kinds?`, `cursor?`, `include_redacted?` |
| `timeline_stats` | timeline_query.rs | Recorder + storage status; row counts by kind/day | `start_ts_ns?`, `end_ts_ns?` (budget-guarded scan; `scan_complete` flag) |
| `timeline_search` | m3_tools.rs | Search timeline by time/app/kind/actor/text | `start_ts_ns`, `end_ts_ns`, `apps?`, `kinds?`, `actor?`, `text?`, `limit`, `cursor?` |
| `timeline_pause` | m3_tools.rs | Pause recorder (survives restart; optional auto-resume) | `duration_ms?` |
| `timeline_resume` | m3_tools.rs | Resume recorder; write session_start boundary | (none) |
| `timeline_exclusions` | m3_tools.rs | List/mutate per-process exclusion list | `add?`, `remove?` |
| `timeline_purge` | m3_tools.rs | Hard-delete matching rows + counts-only audit | `start_ts_ns?`, `end_ts_ns?`, `apps?`, `text?`, `kinds?`, `actor?`, `all?`, `dry_run?`, `cursor?` |
| `timeline_digest` | timeline_digest.rs | Summarize a local day/week of activity | `period` (day/week, req), `date?`, `anchor_ts_ns?`, `include_agent_activity?`, `top_n?` |

## 16.16 Workspace blackboard — `workspace_blackboard.rs`

Run-scoped durable key/value blackboard for multi-agent coordination.

| Tool | Description | Key params | Side effects |
|------|-------------|------------|--------------|
| `workspace_put` | Publish a run-scoped entry (optimistic concurrency, artifact verify) | `run_id?`, `key?`, `expected_version?`, `value?`, `artifact?`, `ttl_ms?` | writes CF_KV; publishes `workspace.put` SSE |
| `workspace_get` | Read one entry by key | `run_id?`, `key?` | read-only (row-hash readback) |
| `workspace_list` | List entries (corruption-isolated scan) | `run_id?`, `prefix?`, `limit?`, `include_values?` | read-only |
| `workspace_subscribe` | Per-session SSE subscription for `workspace.put` | `run_id?`, `prefix?`, `snapshot_first?` | opens SSE subscription |

## 16.17 Tool profiles & notifications — `tool_profiles.rs`, `notify_tools.rs`

| Tool | Description | Key params | Side effects |
|------|-------------|------------|--------------|
| `tool_profile_status` | Read session's effective tool profile + visible tools + routes | none (empty schema) | read-only |
| `tool_profile_set` | Set durable tool profile (normal_agent/browser_control/break_glass) | `profile` (req), `reason?`, `confirm_break_glass?` (break_glass requires reason + held foreground lease) | writes CF_SESSIONS policy |
| `notify_human` | Raise a Windows toast (verified via Action Center; dedupe) | `title`, `body`, `kind` (req), `dedupe_key?`, `suppress_popup?` | OS toast; reads back Action Center |

## 16.18 EverQuest domain pack — `everquest_*.rs` (opt-in)

25 tools, registered only when `SYNAPSE_ENABLE_EVERQUEST` is set. See [12_everquest_domain.md](12_everquest_domain.md). All persist typed redacted rows to CF_KV with exact readback; params summarized. Most take an id + optional `profile_id` + state/source-ref overrides.

| Tool | File | Description |
|------|------|-------------|
| `everquest_chat_input_state` | everquest_tools.rs | Read chat-input pollution state (foreground/UI/OCR). No params. |
| `everquest_loc_probe` | everquest_tools.rs | Send `/loc`, verify appended EQ log coordinate. No params. |
| `everquest_safe_command` | everquest_tools.rs | Send one allowlisted non-social slash command (`command` req). |
| `everquest_survival_readiness` | everquest_tools.rs | Read+persist survival readiness (no input sent). No params. |
| `everquest_autocombat` | everquest_autocombat.rs | Bounded operator-attended combat loop. Params: `max_iterations?`, `max_duration_s?`, `hp_floor_percent?`, `mana_floor_percent?`, `target_level_max?`, `stop_at_level?`, `cast_mana_cost_percent?`, `engagement_timeout_s?`, `hotbar_alias?`, `max_roam_steps?`, `max_chase_s?`, `idempotency_key?`. Sends gameplay input. |
| `everquest_current_state` | everquest_state.rs | Build/persist compact current-state record. No params. |
| `everquest_route_plan` | everquest_route.rs | Plan+persist a bounded route (no movement). `plan_id` req + overrides. |
| `everquest_map_sensor` | everquest_map_sensor.rs | Persist calibrated current-map sensor. `sensor_id` req + overrides. |
| `everquest_planner_guard` | everquest_guard.rs | Evaluate+persist one planner guard decision. `decision_id`, `candidate_kind` req. |
| `everquest_memory_record` | everquest_memory.rs | Persist hazard/safe-area memory row. `memory_id`, `memory_type`, `memory_kind`, `subject`, `confidence` req. |
| `everquest_memory_consult` | everquest_memory.rs | Consult memories for a candidate action. `candidate_id`, `candidate_kind` req. |
| `everquest_world_summary` | everquest_world_summary.rs | Persist compact world-state summary for context injection. |
| `everquest_world_model_record` | everquest_world_model.rs | Persist a world-model row under approved prefix. |
| `everquest_world_model_inspect` | everquest_world_model.rs | Inspect approved world-model prefixes/keys/samples. |
| `everquest_domain_normalize` | everquest_domain.rs | Normalize one state/action/outcome transition. `transition_id`, `state`, `action`, `outcome`, `entity` req. |
| `everquest_outcome_ingest` | everquest_outcome.rs | Parse EQ log bytes into redacted outcome rows. `start_offset?`, `max_bytes=64KB`, `max_events=64`, `log_path?`, `persist_unknown=true`. |
| `everquest_trajectory_record` | everquest_trajectory.rs | Persist one ordered trajectory + JSONL provenance. `trajectory_id`, `intent`, `session_id`, `transitions` req. |
| `everquest_surprise_detect` | everquest_surprise.rs | Compare predicted vs observed; persist surprise row. |
| `everquest_predictive_model_fit` | everquest_predictive_model.rs | Fit transparent predictive baseline. `model_id` req; `min_confidence=0.60`, `max_trajectories=64`. |
| `everquest_predictive_model_predict` | everquest_predictive_model.rs | Predict next outcome w/ calibrated abstention. `prediction_id`, `model_id`, `state_row_key`, `candidate_actions` req. |
| `everquest_action_prior_record` | everquest_scorecard.rs | Persist action-prior prediction/outcome sample. `sample_id`, `prediction_id`, `prediction`, `actual` req. |
| `everquest_action_prior_scorecard` | everquest_scorecard.rs | Aggregate samples into competence scorecard. `window_id` req; `min_samples=3`, floors/targets. |
| `everquest_episode_export` | everquest_episode_export.rs | Export redacted trajectory rows to DynamicJEPA episode JSONL. `export_id`, `trajectory_row_keys` req; `output_path?`, `overwrite=false`. |
| `everquest_contextgraph_ingest` | everquest_contextgraph.rs | Ingest episode JSONL into ContextGraph (external MCP stdio). `ingest_id`, `export_path`, `expected_export_sha256`, `contextgraph_storage_path` req. External call. |
| `everquest_contextgraph_search` | everquest_contextgraph.rs | Query ContextGraph for memories (require provenance). `search_id`, `query`, `contextgraph_storage_path` req. External call. |

---

## 16.19 Notes & caveats

- **Param detail summarized** for: `browser_wait_for` (condition union), `browser_locate`, `profile_registry_query`, `agent_stats`, `session_end`, `target_claim_adopt`, `task_dispatch_once`, `target_act` (full verb-param matrix), `browser_cookies`/`browser_storage` (verb-shaped), and all EverQuest tools. Authoritative field sets live in the named `*Params` structs in each source file.
- **Empty-schema tools** (`input_schema = empty_input_schema()`): `health`, `get_target`, `clear_target`, `control_lease_release`, `control_lease_status`, `tool_profile_status`, `escalation_config_get`.
- Counts: enumerated by `#[tool(...)]` across `crates/synapse-mcp/src/server/`. The 25 EverQuest tools are the gated set in `server.rs:649-667`.
