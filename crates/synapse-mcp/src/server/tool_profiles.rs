use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use rmcp::{ErrorData, RoleServer, model::ErrorCode, model::Tool, service::RequestContext};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use synapse_action::lease;
use synapse_core::error_codes;
use synapse_storage::cf;

use super::{Json, Parameters, SynapseService, empty_input_schema, mcp_error, tool, tool_router};

const TOOL_PROFILE_PREFIX: &str = "mcp/tool-profile/v1/";
const TOOL_PROFILE_SOURCE_OF_TRUTH: &str = "CF_SESSIONS mcp/tool-profile/v1/<session_id>";
const TOOL_PROFILE_ROW_KIND: &str = "mcp_tool_profile";
const TOOL_PROFILE_SCHEMA_VERSION: u32 = 1;
const MAX_PROFILE_REASON_CHARS: usize = 1024;
/// #1559: source of truth for the runtime reality-write opt-in overlay.
const REALITY_WRITE_GRANT_SOURCE_OF_TRUTH: &str =
    "in-memory M3State reality_write_grant overlay + CF_ACTION_LOG audit rows";
/// #1559: the misleading-contract fix, stated on every reality-write status and
/// grant/revoke response so a facade profile is never mistaken for a grant.
const PROFILE_INDEPENDENT_OF_GRANTS_NOTE: &str =
    "the selected facade tool-visibility profile (e.g. full_capability) is INDEPENDENT of M3 permission grants and never yields WRITE_STORAGE by itself; reality-write requires an explicit startup grant (SYNAPSE_MCP_ALLOWED_PERMISSIONS listing WRITE_STORAGE) or a runtime profile operation=grant_reality_write overlay";
pub(crate) const PUBLIC_TOOL_LIMIT: usize = 40;
const PUBLIC_TOOL_REGISTRY_SOURCE_OF_TRUTH: &str =
    "crates/synapse-mcp/src/server/tool_profiles.rs PUBLIC_TOOL_NAMES";
const PUBLIC_TOOL_REGISTRY_OPERATION: &str = "validate_public_tool_registry";
const FACADE_CONTRACT_SOURCE_OF_TRUTH: &str =
    "crates/synapse-mcp/src/server/tool_profiles.rs FACADE_TOOL_CONTRACTS";
const FACADE_CONTRACT_OPERATION: &str = "validate_facade_contract";
const FACADE_CONTRACT_ERROR_CODE: &str = "FACADE_CONTRACT_INVALID";
const FACADE_CONTRACT_STRUCTURED_ERROR: &str = "facade errors must include code, operation, source_of_truth, remediation, and target/source id when applicable";
const CODEX_CLIENT_SURFACE_SOURCE_OF_TRUTH: &str = "%APPDATA%\\synapse\\codex-tool-surface.json + %LOCALAPPDATA%\\synapse\\codex-restart-handoffs + live OS process table";
const CODEX_CLIENT_SURFACE_REMEDIATION: &str = "restart Codex through the patched launcher when a live stale codex.exe PID is named by the latest handoff; rerun scripts\\synapse-setup.ps1 if the host tool-surface snapshot is missing or does not contain the daemon-visible public tools";

pub(crate) const PUBLIC_TOOL_NAMES: &[&str] = &[
    "health",
    "profile",
    "session",
    "subscribe",
    "observe",
    "find",
    "read_text",
    "screenshot",
    "target",
    "act",
    "shell",
    "process",
    "browser_tabs",
    "browser_nav",
    "browser_dom",
    "browser_form",
    "browser_wait",
    "browser_capture",
    "browser_storage",
    "browser_debugger",
    "workspace",
    "agent",
    "task",
    "approval",
    "escalation",
    "timeline",
    "episode",
    "routine",
    "assist",
    "reality",
    "verification",
    "storage",
    "model",
    "cost",
    "hygiene",
    "audit",
    "replay",
    "privacy",
    "setup",
    "telemetry",
];

const PUBLIC_TOOL_IMPLEMENTATION_DENYLIST: &[&str] = &[
    "act_click",
    "act_clipboard",
    "act_combo",
    "act_focus_window",
    "act_keymap",
    "act_launch",
    "act_pad",
    "act_press",
    "act_run_shell",
    "act_run_shell_cancel",
    "act_run_shell_start",
    "act_run_shell_status",
    "act_scroll",
    "act_set_field_text",
    "act_set_value",
    "act_spawn_agent",
    "act_stroke",
    "act_type",
    "agent_ask_operator",
    "agent_inbox",
    "agent_interrupt",
    "agent_kill",
    "agent_pause",
    "agent_cost",
    "agent_cost_price_delete",
    "agent_cost_price_list",
    "agent_cost_price_put",
    "agent_query",
    "agent_receipts",
    "agent_respawn",
    "agent_resume",
    "agent_send",
    "agent_send_broadcast",
    "agent_stats",
    "agent_steer",
    "agent_template_delete",
    "agent_template_get",
    "agent_template_list",
    "agent_template_put",
    "agent_wait",
    "approval_decide",
    "approval_gate",
    "approval_list",
    "approval_request",
    "armed_routine_tick",
    "audit_export_bundle",
    "audit_intelligence_query",
    "browser_screenshot",
    "capture_gif",
    "capture_screenshot",
    "cdp_activate_tab",
    "cdp_bridge_reload",
    "cdp_close_tab",
    "cdp_navigate_tab",
    "cdp_open_tab",
    "cdp_target_info",
    "clear_target",
    "demo_record_start",
    "demo_record_stop",
    "episode_get",
    "episode_list",
    "episode_segment",
    "escalation_ack",
    "escalation_config_get",
    "escalation_config_set",
    "escalation_list",
    "get_target",
    "hidden_desktop_pip_frame",
    "hygiene_flags",
    "hygiene_report",
    "hygiene_scan_storage",
    "hygiene_scan_text",
    "intent_current",
    "intent_detect_tick",
    "local_model_list",
    "local_model_probe",
    "local_model_register",
    "local_model_remove",
    "local_model_update",
    "observe_delta",
    "profile_authoring_generate",
    "profile_authoring_inspect",
    "profile_authoring_list",
    "reality_audit",
    "reality_baseline",
    "release_all",
    "replay_record",
    "routine_automate",
    "routine_feedback",
    "routine_inspect",
    "routine_label_export",
    "routine_list",
    "routine_mine",
    "routine_update",
    "set_target",
    "storage_gc_once",
    "storage_inspect",
    "storage_pressure_sample",
    "storage_put_probe_rows",
    "suggestion_accept",
    "suggestion_list",
    "suggestion_tick",
    "task_cancel",
    "task_claim",
    "task_create",
    "task_dispatch_once",
    "task_get",
    "task_list",
    "task_next",
    "task_reconcile",
    "task_update",
    "target_act",
    "target_claim",
    "target_claim_adopt",
    "target_claim_status",
    "target_release",
    "timeline_digest",
    "timeline_exclusions",
    "timeline_get",
    "timeline_pause",
    "timeline_purge",
    "timeline_redact",
    "timeline_resume",
    "timeline_search",
    "timeline_stats",
    "tool_profile_set",
    "tool_profile_status",
    "verification_audit",
    "verification_bind",
    "verification_inbox",
    "verification_poll",
    "verification_sources",
    "window_list",
    "workspace_get",
    "workspace_list",
    "workspace_put",
    "workspace_subscribe",
];

/// Sentinel `operation_enum` for flat, single-purpose facades that expose NO
/// `operation` discriminator in their input schema (e.g. `health`, `observe`,
/// `find`, `read_text`, `subscribe`). The schema-parity gate
/// `facade_contract_operations_match_live_schema` treats a contract carrying
/// this sentinel as "the built tool must have no `operation` property", and any
/// other `operation_enum` value as "the tool's serialized `operation` enum must
/// exactly equal the contract's declared operation list". Naming a real
/// `*Operation` enum here for a flat tool (as the contracts historically did,
/// with fabricated enum names like `HealthOperation` that never existed) is
/// therefore rejected by the gate. Keep the single operation entry as the
/// documented source-of-truth/error/remediation for the flat call.
pub(crate) const FLAT_FACADE_OPERATION_ENUM: &str =
    "(none: single-purpose tool, no operation param)";

const FACADE_TOOL_CONTRACTS: &[FacadeToolContractSpec] = &[
    facade_contract(
        "health",
        FLAT_FACADE_OPERATION_ENUM,
        "daemon health payload + sanitized tools/list surface",
        &[op(
            "status",
            false,
            false,
            "daemon health payload + process/socket SoT",
            None,
            error_codes::TOOL_INTERNAL_ERROR,
            "read daemon process/socket state, then call health again after fixing the failed subsystem",
        )],
    ),
    facade_contract(
        "profile",
        "ProfileOperation",
        "CF_SESSIONS mcp/tool-profile/v1/<session_id> + sanitized tools/list",
        &[
            op(
                "status",
                false,
                false,
                "CF_SESSIONS policy row + sanitized tools/list",
                None,
                error_codes::TOOL_INTERNAL_ERROR,
                "read tool_profile_status and compare visible tool names against the profile row",
            ),
            op(
                "set",
                true,
                false,
                "CF_SESSIONS mcp/tool-profile/v1/<session_id>",
                Some("CF_SESSIONS row readback + notifications/tools/list_changed attempt"),
                error_codes::TOOL_PROFILE_POLICY_DENIED,
                "hold the required lease/profile permission, provide reason/confirmation, then retry",
            ),
            op(
                "grant_reality_write",
                true,
                false,
                "in-memory M3State reality_write_grant overlay + CF_ACTION_LOG audit rows",
                Some("overlay status readback + CF_ACTION_LOG intent/final audit rows"),
                error_codes::TOOL_PROFILE_POLICY_DENIED,
                "own the foreground input lease (act operation=lease_acquire), then retry with confirm_break_glass=true and a non-empty reason",
            ),
            op(
                "revoke_reality_write",
                true,
                false,
                "in-memory M3State reality_write_grant overlay + CF_ACTION_LOG audit rows",
                Some("overlay status readback (cleared) + CF_ACTION_LOG intent/final audit rows"),
                error_codes::TOOL_INTERNAL_ERROR,
                "provide an MCP session id; revoke is always permitted (de-escalation) and audited",
            ),
        ],
    ),
    facade_contract(
        "session",
        "SessionOperation",
        "MCP session registry + CF_SESSIONS session-target rows",
        &[op(
            "list",
            false,
            false,
            "daemon session registry + CF_SESSIONS session-target rows",
            None,
            error_codes::HTTP_SESSION_INVALID,
            "establish an MCP session and retry through the same client transport",
        )],
    ),
    facade_contract(
        "subscribe",
        FLAT_FACADE_OPERATION_ENUM,
        "SSE subscriber registry + MCP session id",
        &[op(
            "events",
            false,
            false,
            "SSE subscriber registry + emitted event stream",
            None,
            error_codes::HTTP_SESSION_INVALID,
            "open a session-scoped subscription against the live daemon and retry",
        )],
    ),
    facade_contract(
        "observe",
        FLAT_FACADE_OPERATION_ENUM,
        "capture backend readback + perception observation payload",
        &[op(
            "current",
            false,
            true,
            "active session target + capture backend readback",
            None,
            error_codes::CAPTURE_TARGET_INVALID,
            "bind a target or pass an explicit target, then retry observe",
        )],
    ),
    facade_contract(
        "find",
        FLAT_FACADE_OPERATION_ENUM,
        "perception index over latest observation readback",
        &[op(
            "elements",
            false,
            true,
            "latest observation payload + element index",
            None,
            error_codes::CAPTURE_TARGET_INVALID,
            "capture or bind the intended target before querying elements",
        )],
    ),
    facade_contract(
        "read_text",
        FLAT_FACADE_OPERATION_ENUM,
        "OCR/accessibility/browser text readback",
        &[op(
            "text",
            false,
            true,
            "target-specific OCR, accessibility, or browser text readback",
            None,
            error_codes::CAPTURE_TARGET_INVALID,
            "bind the text source target and retry with a scoped selector when needed",
        )],
    ),
    facade_contract(
        "screenshot",
        "ScreenshotOperation",
        "capture artifact path + image metadata readback",
        &[
            op(
                "capture",
                false,
                true,
                "still screenshot artifact bytes + target metadata",
                None,
                error_codes::CAPTURE_TARGET_INVALID,
                "bind the target and retry after the capture backend reports healthy",
            ),
            op(
                "gif",
                false,
                true,
                "GIF artifact bytes + target metadata",
                None,
                error_codes::CAPTURE_TARGET_INVALID,
                "bind the target and retry after the capture backend reports healthy",
            ),
        ],
    ),
    facade_contract(
        "target",
        "TargetOperation",
        "MCP session target registry + CF_SESSIONS row",
        &[
            op(
                "get",
                false,
                false,
                "daemon session target registry + CF_SESSIONS row",
                None,
                error_codes::HTTP_SESSION_INVALID,
                "establish an MCP session before reading target state",
            ),
            op(
                "list",
                false,
                false,
                "top-level window snapshot + target claim registry readback",
                None,
                error_codes::ACTION_TARGET_INVALID,
                "refresh the live window list and choose a current target row",
            ),
            op(
                "set",
                true,
                true,
                "daemon session target registry + CF_SESSIONS row",
                Some("CF_SESSIONS session-target row readback + daemon registry readback"),
                error_codes::ACTION_TARGET_INVALID,
                "pass a valid window or CDP target discovered from the live host",
            ),
            op(
                "clear",
                true,
                false,
                "daemon session target registry + CF_SESSIONS row",
                Some("CF_SESSIONS session-target row readback + daemon registry readback"),
                error_codes::HTTP_SESSION_INVALID,
                "establish an MCP session before clearing target state",
            ),
            op(
                "claim",
                true,
                true,
                "target claim registry + CF_ACTION_LOG",
                Some("target claim registry readback + CF_ACTION_LOG command audit row"),
                error_codes::ACTION_TARGET_INVALID,
                "pass a live target from target operation=list, then retry claim",
            ),
            op(
                "status",
                false,
                false,
                "target claim registry readback",
                None,
                error_codes::HTTP_SESSION_INVALID,
                "establish an MCP session before reading target claim status",
            ),
            op(
                "adopt",
                true,
                true,
                "target claim registry + session lifecycle state",
                Some("target claim registry readback + owner session teardown readback"),
                error_codes::TARGET_CLAIM_NOT_FOUND,
                "read target operation=status, then pass a current owner_session_id",
            ),
            op(
                "release",
                true,
                true,
                "target claim registry + CF_ACTION_LOG",
                Some("target claim registry readback + CF_ACTION_LOG command audit row"),
                error_codes::TARGET_CLAIM_NOT_FOUND,
                "pass the claimed target or current session claim before releasing",
            ),
        ],
    ),
    facade_contract(
        "act",
        "ActOperation",
        "act facade CF_ACTION_LOG command audit row + target/action audit row + post-action target readback",
        &[
            op(
                "invoke",
                true,
                true,
                "target action preflight + input lease/readback",
                Some("act facade CF_ACTION_LOG command audit row + post-action target/UI readback"),
                error_codes::ACTION_TARGET_INVALID,
                "bind a valid target and acquire any required control lease before retrying",
            ),
            op(
                "foreground",
                true,
                true,
                "foreground input lease + target/action audit row",
                Some("foreground lease/profile restore readback + post-action target/UI readback"),
                error_codes::ACTION_FOREGROUND_LEASE_NOT_HELD,
                "provide a non-empty reason and let the facade acquire/restore the audited foreground lane",
            ),
            op(
                "lease_acquire",
                true,
                false,
                "synapse_action input lease + CF_SESSIONS session lease row + act facade CF_ACTION_LOG command audit row",
                Some(
                    "lease status readback + CF_SESSIONS persisted lease row + act facade CF_ACTION_LOG command audit row",
                ),
                error_codes::ACTION_FOREGROUND_LEASE_BUSY,
                "retry after the reported holder expires/releases, or call act operation=lease_status to inspect the owner",
            ),
            op(
                "lease_status",
                false,
                false,
                "synapse_action input lease + CF_SESSIONS persisted lease row + act facade CF_ACTION_LOG command audit row",
                Some("lease status readback + act facade CF_ACTION_LOG command audit row"),
                error_codes::HTTP_SESSION_INVALID,
                "establish an MCP session before reading lease state",
            ),
            op(
                "lease_release",
                true,
                false,
                "synapse_action input lease + CF_SESSIONS persisted lease row + act facade CF_ACTION_LOG command audit row",
                Some(
                    "lease status readback + CF_SESSIONS persisted lease row deletion + act facade CF_ACTION_LOG command audit row",
                ),
                error_codes::ACTION_FOREGROUND_LEASE_NOT_HELD,
                "only the owning session can release; call act operation=lease_status to inspect the owner",
            ),
        ],
    ),
    facade_contract(
        "shell",
        "ShellOperation",
        "%LOCALAPPDATA%\\Synapse\\shell-jobs + %LOCALAPPDATA%\\Synapse\\shell-sessions + daemon-tool-events.jsonl",
        &[
            op(
                "run",
                true,
                false,
                "durable shell registry/log files or inline child process",
                Some("durable job row/status readback + output artifact path when backgrounded"),
                error_codes::TOOL_PARAMS_INVALID,
                "provide a non-empty executable in command and put arguments in args",
            ),
            op(
                "start",
                true,
                false,
                "durable shell job registry + process table",
                Some(
                    "job status/stdout/stderr artifact paths read back from %LOCALAPPDATA%\\Synapse\\shell-jobs",
                ),
                error_codes::TOOL_PARAMS_INVALID,
                "provide command/args for the durable job and poll with status",
            ),
            op(
                "status",
                false,
                false,
                "durable shell job registry + output artifact path",
                None,
                error_codes::TOOL_PARAMS_INVALID,
                "provide the exact job_id returned by run/start",
            ),
            op(
                "cancel",
                true,
                false,
                "durable shell job registry + recorded process tree",
                Some("before/after status readback plus remaining process ids"),
                error_codes::TOOL_PARAMS_INVALID,
                "provide the exact job_id returned by run/start",
            ),
        ],
    ),
    facade_contract(
        "process",
        "ProcessOperation",
        "live OS process table + CF_PROCESS_HISTORY",
        &[
            op(
                "list",
                false,
                false,
                "live OS process table snapshot",
                None,
                error_codes::TOOL_PARAMS_INVALID,
                "scope the query by exact pid, process_name_contains, or command_line_contains",
            ),
            op(
                "launch",
                true,
                true,
                "process table plus CF_PROCESS_HISTORY/session lifecycle resources",
                Some("CF_PROCESS_HISTORY row plus live process table readback"),
                error_codes::TOOL_PARAMS_INVALID,
                "provide a non-empty target executable/path and use process history/list for readback",
            ),
            op(
                "history",
                false,
                false,
                "CF_PROCESS_HISTORY tail rows",
                None,
                error_codes::TOOL_PARAMS_INVALID,
                "read the launch pid/target from CF_PROCESS_HISTORY or filter by a known pid",
            ),
        ],
    ),
    facade_contract(
        "browser_tabs",
        "BrowserTabsOperation",
        "already-open Chrome bridge tabs.query/readback",
        &[
            op(
                "list",
                false,
                false,
                "Chrome bridge tabs.query result + window context",
                None,
                error_codes::ACTION_TARGET_INVALID,
                "use an already-open authenticated Chrome window and retry",
            ),
            op(
                "select",
                true,
                true,
                "Chrome bridge tabs.query result + MCP session target registry",
                Some("session target registry readback after selecting the tab"),
                error_codes::ACTION_TARGET_INVALID,
                "select a cdp_target_id from the current tabs list",
            ),
            op(
                "activate",
                true,
                false,
                "Chrome bridge tabs.update(active=true) result + tabs.query readback",
                Some("tabs.query active/highlighted readback for the requested target"),
                error_codes::ACTION_TARGET_INVALID,
                "activate a cdp_target_id from the requested window's current tabs list",
            ),
            op(
                "new",
                true,
                false,
                "Chrome bridge tabs.create result + tabs.query readback",
                Some("new tab id readback from tabs.query"),
                error_codes::TOOL_PARAMS_INVALID,
                "pass a valid URL or empty string for about:blank",
            ),
            op(
                "close",
                true,
                true,
                "Chrome bridge tabs.remove result + tabs.query readback",
                Some("tabs.query absence readback for the closed target"),
                error_codes::ACTION_TARGET_INVALID,
                "close only a tab owned by this MCP session",
            ),
        ],
    ),
    facade_contract(
        "browser_nav",
        "BrowserNavOperation",
        "Chrome bridge navigation result + page URL/readiness readback",
        &[
            op(
                "navigate",
                true,
                true,
                "Chrome bridge/CDP navigation command",
                Some("page URL + readyState readback from the same target"),
                error_codes::TOOL_PARAMS_INVALID,
                "pass a valid target-scoped URL and wait condition",
            ),
            op(
                "reload",
                true,
                true,
                "Chrome bridge/CDP reload command",
                Some("page URL + readyState readback from the same target"),
                error_codes::ACTION_TARGET_INVALID,
                "select or open an owned target, then retry reload",
            ),
            op(
                "back",
                true,
                true,
                "Chrome bridge/CDP navigation history command",
                Some("page URL + readyState readback from the same target"),
                error_codes::ACTION_TARGET_INVALID,
                "select or open an owned target with navigation history, then retry back",
            ),
            op(
                "forward",
                true,
                true,
                "Chrome bridge/CDP navigation history command",
                Some("page URL + readyState readback from the same target"),
                error_codes::ACTION_TARGET_INVALID,
                "select or open an owned target with forward history, then retry forward",
            ),
        ],
    ),
    facade_contract(
        "browser_dom",
        "BrowserDomOperation",
        "target-scoped DOM/ARIA readback",
        &[
            op(
                "content",
                false,
                true,
                "target-scoped document HTML readback",
                None,
                error_codes::ACTION_TARGET_INVALID,
                "bind the intended tab and retry content readback",
            ),
            op(
                "locate",
                false,
                true,
                "target-scoped DOM locator readback",
                None,
                error_codes::ACTION_TARGET_INVALID,
                "bind the intended tab and use a non-empty strict selector or locator",
            ),
            op(
                "inspect",
                false,
                true,
                "target-scoped element property/actionability readback",
                None,
                error_codes::ACTION_TARGET_INVALID,
                "pass an element_id returned from browser_dom operation=locate on the same target",
            ),
            op(
                "aria_snapshot",
                false,
                true,
                "target-scoped accessibility tree readback",
                None,
                error_codes::ACTION_TARGET_INVALID,
                "bind the intended tab and keep root element ids target-scoped",
            ),
        ],
    ),
    facade_contract(
        "browser_form",
        "BrowserFormOperation",
        "target-scoped DOM form mutation + DOM value readback",
        &[
            op(
                "set_value",
                true,
                true,
                "target-scoped DOM mutation",
                Some("DOM value/property readback after mutation"),
                error_codes::ACTION_TARGET_INVALID,
                "bind the target and pass a strict selector or element id",
            ),
            op(
                "fill",
                true,
                true,
                "ordered target-scoped DOM form mutations",
                Some("per-field DOM value/property readback after mutation"),
                error_codes::ACTION_TARGET_INVALID,
                "bind the target and pass one or more strict field specs",
            ),
        ],
    ),
    facade_contract(
        "browser_wait",
        "BrowserWaitOperation",
        "target-scoped browser wait condition readback",
        &[op(
            "for_condition",
            false,
            true,
            "target-scoped DOM/URL/load-state readback",
            None,
            error_codes::TOOL_PARAMS_INVALID,
            "use a declared wait condition and bounded timeout",
        )],
    ),
    facade_contract(
        "browser_capture",
        "BrowserCaptureOperation",
        "browser screenshot/download artifacts and readbacks",
        &[
            op(
                "screenshot",
                false,
                true,
                "target-scoped screenshot bytes + page metadata",
                None,
                error_codes::ACTION_TARGET_INVALID,
                "bind the tab and retry capture after the bridge reports healthy",
            ),
            op(
                "downloads",
                true,
                false,
                "Chrome downloads rows/events or saved file bytes",
                Some("saved file path/bytes/hash or Chrome downloads event cursor"),
                error_codes::TOOL_PARAMS_INVALID,
                "pass downloads={...} with a bounded wait/filter and verify file/event readback",
            ),
        ],
    ),
    facade_contract(
        "browser_storage",
        "BrowserStorageOperation",
        "session-owned chrome-tab local/session storage + Playwright storageState readback",
        &[
            op(
                "get",
                false,
                true,
                "target-scoped localStorage/sessionStorage readback via chrome.scripting",
                None,
                error_codes::ACTION_TARGET_INVALID,
                "bind a session-owned chrome-tab target and request the local or session store",
            ),
            op(
                "set",
                true,
                true,
                "target-scoped localStorage/sessionStorage after write",
                Some("target-scoped localStorage/sessionStorage readback after the set"),
                error_codes::TOOL_PARAMS_INVALID,
                "pass a non-empty key plus value and verify the post-write readback",
            ),
            op(
                "clear",
                true,
                true,
                "target-scoped localStorage/sessionStorage after removal",
                Some("target-scoped localStorage/sessionStorage readback after the clear"),
                error_codes::ACTION_TARGET_INVALID,
                "bind a session-owned chrome-tab target, then verify the store/key is absent",
            ),
            op(
                "save_state",
                false,
                true,
                "exported Playwright-style storageState (cookies + per-origin localStorage) readback",
                None,
                error_codes::ACTION_TARGET_INVALID,
                "bind a session-owned chrome-tab target and read the exported storageState object",
            ),
            op(
                "load_state",
                true,
                true,
                "target-scoped cookies + localStorage after applying storageState",
                Some("target-scoped storage readback after storageState import"),
                error_codes::TOOL_PARAMS_INVALID,
                "pass a valid storageState object and verify the imported cookies/localStorage readback",
            ),
        ],
    ),
    facade_contract(
        "browser_debugger",
        "BrowserDebuggerOperation",
        "explicit browser_debugger profile + raw CDP/chrome.debugger readback",
        &[
            op(
                "evaluate",
                true,
                true,
                "browser_debugger profile row + Runtime.evaluate response",
                Some("Runtime.evaluate target/url/ready_state/value readback"),
                error_codes::TOOL_PROFILE_POLICY_DENIED,
                "switch to browser_debugger with reason/confirmation before evaluation",
            ),
            op(
                "console_messages",
                false,
                true,
                "browser_debugger profile row + console buffer cursor",
                None,
                error_codes::TOOL_PROFILE_POLICY_DENIED,
                "switch to browser_debugger and read the target console buffer",
            ),
            op(
                "reload_bridge",
                true,
                false,
                "browser_debugger profile row + chrome.runtime.reload bridge command",
                Some("bridge host before/after registration readback"),
                error_codes::TOOL_PROFILE_POLICY_DENIED,
                "switch to browser_debugger and verify the bridge host reconnect readback",
            ),
            op(
                "pdf",
                true,
                true,
                "browser_debugger profile row + PDF file path",
                Some("PDF file bytes/hash readback"),
                error_codes::TOOL_PROFILE_POLICY_DENIED,
                "switch to browser_debugger and verify the PDF file bytes/hash",
            ),
            op(
                "file_upload",
                true,
                true,
                "browser_debugger profile row + file input/chooser readback",
                Some("file input/chooser state readback"),
                error_codes::TOOL_PROFILE_POLICY_DENIED,
                "switch to browser_debugger and verify input or chooser state",
            ),
            op(
                "dialog",
                true,
                true,
                "browser_debugger profile row + dialog buffer readback",
                Some("dialog pending/history readback"),
                error_codes::TOOL_PROFILE_POLICY_DENIED,
                "switch to browser_debugger and verify dialog pending/history state",
            ),
            op(
                "add_init_script",
                true,
                true,
                "browser_debugger profile row + init-script identifier",
                Some("init-script identifier/readback for the target"),
                error_codes::TOOL_PROFILE_POLICY_DENIED,
                "switch to browser_debugger and verify returned script identifier",
            ),
            op(
                "add_script_tag",
                true,
                true,
                "browser_debugger profile row + script tag injection readback",
                Some("script tag target/source readback"),
                error_codes::TOOL_PROFILE_POLICY_DENIED,
                "switch to browser_debugger and verify script tag target/source",
            ),
            op(
                "add_style_tag",
                true,
                true,
                "browser_debugger profile row + style tag injection readback",
                Some("style tag target/source readback"),
                error_codes::TOOL_PROFILE_POLICY_DENIED,
                "switch to browser_debugger and verify style tag target/source",
            ),
            op(
                "network",
                false,
                true,
                "browser_debugger profile row + captured network rows",
                None,
                error_codes::TOOL_PROFILE_POLICY_DENIED,
                "switch to browser_debugger and read the captured network rows",
            ),
            op(
                "network_har",
                true,
                true,
                "browser_debugger profile row + HAR file/routes",
                Some("HAR file bytes or replay route readback"),
                error_codes::TOOL_PROFILE_POLICY_DENIED,
                "switch to browser_debugger and verify HAR bytes/routes",
            ),
            op(
                "network_overrides",
                true,
                true,
                "browser_debugger profile row + network override state",
                Some("target-scoped header/User-Agent override readback"),
                error_codes::TOOL_PROFILE_POLICY_DENIED,
                "switch to browser_debugger and verify override state",
            ),
            op(
                "route",
                true,
                true,
                "browser_debugger profile row + route state",
                Some("target-scoped route table readback"),
                error_codes::TOOL_PROFILE_POLICY_DENIED,
                "switch to browser_debugger and verify route table state",
            ),
            op(
                "emulate",
                true,
                true,
                "browser_debugger profile row + emulation domain state",
                Some("target-scoped emulation result readback"),
                error_codes::TOOL_PROFILE_POLICY_DENIED,
                "switch to browser_debugger and verify emulation domain result",
            ),
            op(
                "expose_binding",
                true,
                true,
                "browser_debugger profile row + binding buffer/state",
                Some("binding active/cursor readback"),
                error_codes::TOOL_PROFILE_POLICY_DENIED,
                "switch to browser_debugger and verify binding state",
            ),
            op(
                "drag",
                true,
                true,
                "browser_debugger profile row + drag target readback",
                Some("drag status/readback for the same target"),
                error_codes::TOOL_PROFILE_POLICY_DENIED,
                "switch to browser_debugger and verify drag target status",
            ),
            op(
                "drop",
                true,
                true,
                "browser_debugger profile row + drop target readback",
                Some("drop status/readback for the same target"),
                error_codes::TOOL_PROFILE_POLICY_DENIED,
                "switch to browser_debugger and verify drop target status",
            ),
        ],
    ),
    facade_contract(
        "workspace",
        "WorkspaceOperation",
        "CF_KV workspace-blackboard exact rows",
        &[
            op(
                "get",
                false,
                false,
                "CF_KV workspace-blackboard exact row",
                None,
                "WORKSPACE_KEY_ABSENT",
                "read the exact run/key row and handle absent rows explicitly",
            ),
            op(
                "put",
                true,
                false,
                "CF_KV workspace-blackboard exact row",
                Some("CF_KV exact row readback with value hash/version"),
                error_codes::STORAGE_WRITE_FAILED,
                "retry with the observed expected_version or correct the key/value",
            ),
            op(
                "list",
                false,
                false,
                "CF_KV workspace-blackboard run/prefix scan",
                None,
                error_codes::STORAGE_READ_FAILED,
                "provide a valid run_id/prefix/limit and inspect corrupt_rows_skipped",
            ),
            op(
                "subscribe",
                true,
                false,
                "SSE subscription registry + workspace.put event stream",
                Some("subscription id + SSE registry readback"),
                error_codes::TOOL_PARAMS_INVALID,
                "provide a non-empty prefix and read the returned subscription_id",
            ),
            op(
                "exists",
                false,
                false,
                "CF_KV workspace-blackboard exact row or absent readback",
                None,
                "WORKSPACE_KEY_ABSENT",
                "use exists to distinguish absent user keys from storage failures",
            ),
            op(
                "delete",
                true,
                false,
                "CF_KV workspace-blackboard exact row",
                Some("pre-delete row hash/version or corrupt hash + post-delete absent readback"),
                error_codes::STORAGE_WRITE_FAILED,
                "read the row version or corrupt hash, pass the matching guard, then verify the row is absent",
            ),
            op(
                "wait",
                false,
                false,
                "CF_KV workspace-blackboard exact row polled until present or timeout",
                None,
                "WORKSPACE_WAIT_TIMEOUT",
                "increase timeout_ms or ensure a peer publishes the key with workspace put",
            ),
        ],
    ),
    facade_contract(
        "agent",
        "AgentOperation",
        "%LOCALAPPDATA%\\synapse\\agent-spawns + CF_AGENT_EVENTS/CF_AGENT_TRANSCRIPTS + CF_KV mailbox/template rows",
        &[
            op(
                "spawn",
                true,
                false,
                "agent spawn directory + session registry + CF_AGENT_EVENTS",
                Some("spawned agent directory, readiness artifact, session row, and event rows"),
                error_codes::TOOL_INTERNAL_ERROR,
                "fix the direct spawn/template request and inspect the spawn artifacts before retrying",
            ),
            op(
                "query",
                false,
                false,
                "CF_AGENT_EVENTS + CF_AGENT_TRANSCRIPTS scan",
                None,
                error_codes::TOOL_INTERNAL_ERROR,
                "provide a real MCP session id or agent-spawn id and inspect scan_readback",
            ),
            op(
                "send",
                true,
                false,
                "CF_KV mailbox row",
                Some("mailbox row readback with row key/length/hash"),
                error_codes::TOOL_INTERNAL_ERROR,
                "resolve the recipient to a live MCP session and inspect the mailbox row",
            ),
            op(
                "inbox",
                true,
                false,
                "CF_KV mailbox scan",
                Some("returned/deleted mailbox rows"),
                error_codes::TOOL_INTERNAL_ERROR,
                "inspect this session's mailbox rows and retry with a valid filter",
            ),
            op(
                "wait",
                false,
                false,
                "CF_KV mailbox wait/read scan",
                None,
                error_codes::TOOL_INTERNAL_ERROR,
                "inspect timeout_ms and mailbox rows before retrying",
            ),
            op(
                "broadcast",
                true,
                false,
                "CF_KV mailbox rows per recipient",
                Some("per-recipient delivered/skipped row readbacks"),
                error_codes::TOOL_INTERNAL_ERROR,
                "inspect resolved recipients and per-recipient outcomes",
            ),
            op(
                "receipts",
                true,
                false,
                "CF_KV receipt-box rows",
                Some("returned/deleted receipt rows"),
                error_codes::TOOL_INTERNAL_ERROR,
                "inspect this session's receipt rows before retrying",
            ),
            op(
                "stats",
                false,
                false,
                "CF_AGENT_EVENTS budget-guarded stats scan",
                None,
                error_codes::TOOL_INTERNAL_ERROR,
                "inspect scan bounds and group_by before retrying",
            ),
            op(
                "template_put",
                true,
                false,
                "CF_KV agent template rows",
                Some("template row key/length/hash readbacks"),
                error_codes::TOOL_INTERNAL_ERROR,
                "fix template_id/model/prompt and inspect written template rows",
            ),
            op(
                "template_get",
                false,
                false,
                "CF_KV agent template row",
                None,
                error_codes::TOOL_INTERNAL_ERROR,
                "provide an existing template_id",
            ),
            op(
                "template_list",
                false,
                false,
                "CF_KV agent template prefix scan",
                None,
                error_codes::TOOL_INTERNAL_ERROR,
                "inspect the template prefix scan limit",
            ),
            op(
                "template_delete",
                true,
                false,
                "CF_KV agent template row",
                Some("deleted template row key"),
                error_codes::TOOL_INTERNAL_ERROR,
                "provide an existing template_id and verify the row is absent after delete",
            ),
            op(
                "task_started",
                true,
                false,
                "%LOCALAPPDATA%\\synapse\\agent-spawns task-started.json + MCP session id",
                Some("task-started artifact path/session/readiness source"),
                error_codes::TOOL_INTERNAL_ERROR,
                "provide the daemon-issued spawn_id from the spawned MCP session",
            ),
            op(
                "interrupt",
                true,
                false,
                "agent clean-channel outcomes + process table readback",
                Some("per-channel delivery/process readback"),
                error_codes::TOOL_INTERNAL_ERROR,
                "inspect channel outcomes and process state",
            ),
            op(
                "kill",
                true,
                false,
                "agent process tree + CF_AGENT_EVENTS",
                Some("before/after process table readback"),
                error_codes::TOOL_INTERNAL_ERROR,
                "inspect process readback and killed event rows",
            ),
            op(
                "steer",
                true,
                false,
                "agent clean-channel outcomes + mailbox/receipt rows",
                Some("per-channel delivery readback"),
                error_codes::TOOL_INTERNAL_ERROR,
                "inspect steering channels and receipt/mailbox rows",
            ),
            op(
                "pause",
                true,
                false,
                "agent process/thread table readback",
                Some("thread suspension readback"),
                error_codes::TOOL_INTERNAL_ERROR,
                "inspect per-process thread suspension state",
            ),
            op(
                "resume",
                true,
                false,
                "agent process/thread table readback",
                Some("thread resume readback"),
                error_codes::TOOL_INTERNAL_ERROR,
                "inspect per-process thread running state",
            ),
            op(
                "respawn",
                true,
                false,
                "prior spawn manifest + new spawn directory + lineage event rows",
                Some("new spawn directory/session/readiness readback"),
                error_codes::TOOL_INTERNAL_ERROR,
                "inspect the prior spawn manifest and new spawn artifacts",
            ),
        ],
    ),
    facade_contract(
        "task",
        "TaskOperation",
        "CF_KV agent task rows + task event/readback rows",
        &[
            op(
                "create",
                true,
                false,
                "CF_KV agent task row",
                Some("written task row readback"),
                error_codes::TOOL_INTERNAL_ERROR,
                "fix task_id/template_id/title and inspect the written row",
            ),
            op(
                "get",
                false,
                false,
                "CF_KV agent task row",
                None,
                error_codes::TOOL_INTERNAL_ERROR,
                "provide an existing task_id",
            ),
            op(
                "update",
                true,
                false,
                "CF_KV agent task row",
                Some("written task row readback"),
                error_codes::TOOL_INTERNAL_ERROR,
                "read the current state and use a valid transition",
            ),
            op(
                "claim",
                true,
                false,
                "CF_KV agent task row",
                Some("written task row readback"),
                error_codes::TOOL_INTERNAL_ERROR,
                "claim only todo tasks with a real agent session id",
            ),
            op(
                "cancel",
                true,
                false,
                "CF_KV agent task row",
                Some("terminal task row readback"),
                error_codes::TOOL_INTERNAL_ERROR,
                "cancel only non-terminal tasks",
            ),
            op(
                "list",
                false,
                false,
                "CF_KV agent task prefix scan",
                None,
                error_codes::TOOL_INTERNAL_ERROR,
                "inspect the state/max filter and reconciled orphan list",
            ),
            op(
                "next",
                false,
                false,
                "CF_KV agent task queue scan",
                None,
                error_codes::TOOL_INTERNAL_ERROR,
                "inspect in-flight tasks and concurrency cap",
            ),
            op(
                "reconcile",
                true,
                false,
                "CF_KV task rows + agent spawn completion artifacts",
                Some("orphan/settled task row readbacks"),
                error_codes::TOOL_INTERNAL_ERROR,
                "inspect in-progress rows, spawn artifacts, and live sessions",
            ),
            op(
                "dispatch_once",
                true,
                false,
                "CF_KV task row + agent spawn directory + readiness artifact",
                Some("spawn directory/session/readiness or failed attempt row"),
                error_codes::TOOL_INTERNAL_ERROR,
                "inspect task row, spawn directory, readiness artifact, and failed attempt record",
            ),
        ],
    ),
    facade_contract(
        "approval",
        "ApprovalOperation",
        "CF_KV approval/v1/item rows + approval/v1/audit rows + daemon-tool-events.jsonl",
        &[
            op(
                "request",
                true,
                false,
                "CF_KV approval item row + approval audit row",
                Some("approval item_row/audit_row readback"),
                error_codes::TOOL_INTERNAL_ERROR,
                "fix request fields and inspect item_row/audit_row in CF_KV",
            ),
            op(
                "list",
                false,
                false,
                "CF_KV approval item prefix scan",
                None,
                error_codes::TOOL_INTERNAL_ERROR,
                "read current approval rows and choose an existing request id",
            ),
            op(
                "decide",
                true,
                false,
                "CF_KV approval item row + transition audit row",
                Some("approval decision item_row/audit_row readback"),
                error_codes::TOOL_PARAMS_INVALID,
                "decide an existing pending approval id with explicit outcome",
            ),
            op(
                "gate",
                true,
                false,
                "CF_KV approval item row for risky calls or direct gate verdict for auto-allow",
                Some("approval gate verdict plus queue row when one is created"),
                error_codes::TOOL_PARAMS_INVALID,
                "provide a strict permission-prompt payload and inspect the gate verdict/queue row",
            ),
            op(
                "ask_operator",
                true,
                false,
                "CF_KV agent_question approval row + audit row",
                Some("operator answer/decline/timeout row readback"),
                error_codes::TOOL_PARAMS_INVALID,
                "provide a non-empty question and inspect the agent_question row",
            ),
        ],
    ),
    facade_contract(
        "escalation",
        "EscalationOperation",
        "CF_KV escalation/v1/config + escalation/v1/item rows + escalation/v1/audit rows",
        &[
            op(
                "config_get",
                false,
                false,
                "CF_KV escalation/v1/config row or absent-row default",
                None,
                error_codes::TOOL_INTERNAL_ERROR,
                "inspect the persisted config row or default policy readback",
            ),
            op(
                "config_set",
                true,
                false,
                "CF_KV escalation/v1/config row",
                Some("persisted config row readback"),
                error_codes::TOOL_PARAMS_INVALID,
                "fix escalation policy fields and inspect the config row after write",
            ),
            op(
                "list",
                false,
                false,
                "CF_KV escalation item prefix scan",
                None,
                error_codes::TOOL_INTERNAL_ERROR,
                "inspect status/anchor filters and item rows",
            ),
            op(
                "ack",
                true,
                false,
                "CF_KV escalation item row + audit row",
                Some("acked item/audit row readback"),
                error_codes::TOOL_PARAMS_INVALID,
                "provide an existing escalation_id and inspect item/audit rows after ack",
            ),
        ],
    ),
    facade_contract(
        "timeline",
        "TimelineOperation",
        "CF_TIMELINE rows + live recorder control gate",
        &[
            op(
                "get",
                false,
                false,
                "CF_TIMELINE ordered row scan",
                None,
                error_codes::STORAGE_READ_FAILED,
                "narrow the time range/kind/actor filter and inspect CF_TIMELINE health",
            ),
            op(
                "search",
                false,
                false,
                "CF_TIMELINE filtered row scan",
                None,
                error_codes::STORAGE_READ_FAILED,
                "narrow the time/text/app filter and inspect CF_TIMELINE health",
            ),
            op(
                "stats",
                false,
                false,
                "CF_TIMELINE aggregate scan + recorder control gate",
                None,
                error_codes::STORAGE_READ_FAILED,
                "fix the stats bounds and inspect recorder control state plus CF_TIMELINE rows",
            ),
        ],
    ),
    facade_contract(
        "episode",
        "EpisodeOperation",
        "CF_EPISODES rows + CF_TIMELINE evidence refs",
        &[
            op(
                "list",
                false,
                false,
                "CF_EPISODES bounded row scan",
                None,
                error_codes::STORAGE_READ_FAILED,
                "run episode segmentation if no rows exist, or narrow the time/app/actor filter",
            ),
            op(
                "get",
                false,
                false,
                "CF_EPISODES episode row + CF_TIMELINE evidence refs",
                None,
                error_codes::STORAGE_READ_FAILED,
                "provide an existing episode_id and inspect CF_EPISODES plus CF_TIMELINE refs",
            ),
        ],
    ),
    facade_contract(
        "routine",
        "RoutineOperation",
        "CF_ROUTINES + CF_ROUTINE_STATE + CF_KV routine automation/armed rows",
        &[
            op(
                "mine",
                true,
                false,
                "CF_EPISODES input rows + CF_ROUTINES replace-all rows + CF_ROUTINE_STATE reconciliation rows",
                Some("routines_written/deleted and state row count readback"),
                error_codes::STORAGE_READ_FAILED,
                "fix mining bounds and inspect CF_EPISODES, CF_ROUTINES, and CF_ROUTINE_STATE",
            ),
            op(
                "list",
                false,
                false,
                "CF_ROUTINES joined with CF_ROUTINE_STATE",
                None,
                error_codes::STORAGE_READ_FAILED,
                "read routine storage health and retry with a narrower filter",
            ),
            op(
                "inspect",
                false,
                false,
                "CF_ROUTINES exact routine row + CF_ROUTINE_STATE row + automation/armed rows",
                None,
                error_codes::STORAGE_READ_FAILED,
                "provide an existing routine_id and inspect routine state rows",
            ),
            op(
                "update",
                true,
                false,
                "CF_ROUTINE_STATE row + optional CF_KV armed_routine/v1 row",
                Some("routine state row and armed row readback"),
                error_codes::TOOL_PARAMS_INVALID,
                "fix lifecycle/arming params and inspect routine state after write",
            ),
            op(
                "feedback",
                true,
                false,
                "CF_ROUTINE_STATE feedback counters/history",
                Some("routine feedback state row readback"),
                error_codes::TOOL_PARAMS_INVALID,
                "provide an existing routine_id/outcome and inspect feedback history",
            ),
            op(
                "label",
                false,
                false,
                "CF_ROUTINES + CF_ROUTINE_STATE naming evidence",
                None,
                error_codes::STORAGE_READ_FAILED,
                "provide an existing routine_id and inspect label evidence rows",
            ),
            op(
                "automate",
                true,
                false,
                "CF_KV profile_authoring/v1 candidate + routine_automation/v1 row + plan/v1 row",
                Some("candidate, automation, and plan row readback"),
                error_codes::TOOL_PARAMS_INVALID,
                "provide a mined routine_id and inspect authoring/automation rows",
            ),
            op(
                "armed_tick",
                true,
                false,
                "CF_KV armed_routine/v1 + armed_routine_run/v1 + plan_execution/v1 rows",
                Some("armed tick run/audit rows readback"),
                error_codes::TOOL_PARAMS_INVALID,
                "fix armed tick filters and inspect armed routine run rows",
            ),
        ],
    ),
    facade_contract(
        "assist",
        "AssistOperation",
        "CF_KV suggestion/v1 + intent tracker/events + CF_ROUTINES/CF_ROUTINE_STATE",
        &[
            op(
                "intent",
                false,
                false,
                "CF_EPISODES recent rows + CF_ROUTINES/CF_ROUTINE_STATE",
                None,
                error_codes::STORAGE_READ_FAILED,
                "fix intent filters and inspect derived episode/routine rows",
            ),
            op(
                "detect",
                false,
                false,
                "intent tracker state + event bus delivery counts + routine stores",
                None,
                error_codes::STORAGE_READ_FAILED,
                "fix detection filters and inspect intent tracker/event delivery",
            ),
            op(
                "suggestion_tick",
                true,
                false,
                "CF_KV suggestion/v1 rows + suggestion feedback rows",
                Some("created/expired/abandoned suggestion row readback"),
                error_codes::STORAGE_READ_FAILED,
                "fix suggestion tick bounds and inspect suggestion rows",
            ),
            op(
                "suggestion_list",
                false,
                false,
                "CF_KV suggestion/v1 prefix scan",
                None,
                error_codes::STORAGE_READ_FAILED,
                "narrow suggestion filters and inspect suggestion rows",
            ),
            op(
                "suggestion_accept",
                true,
                true,
                "CF_KV suggestion/v1 + plan/v1 + plan_execution/v1 rows",
                Some("accepted suggestion and plan execution row readback"),
                error_codes::ACTION_TARGET_INVALID,
                "bind any required browser target and inspect suggestion/plan rows",
            ),
        ],
    ),
    facade_contract(
        "reality",
        "RealityOperation",
        "CF_KV reality baseline/delta/audit rows + physical observation readback",
        &[
            op(
                "baseline",
                true,
                true,
                "CF_KV reality head/baseline rows + physical observation readback",
                Some("reality head/baseline row readback"),
                error_codes::STORAGE_READ_FAILED,
                "fix baseline profile/epoch params and inspect reality rows",
            ),
            op(
                "delta",
                true,
                true,
                "CF_KV reality delta/head rows + physical observation readback",
                Some("delta rows and cursor/head readback"),
                error_codes::STORAGE_READ_FAILED,
                "fix delta cursor/profile params and inspect reality delta rows",
            ),
            op(
                "audit",
                true,
                true,
                "CF_KV reality audit/head rows + fresh physical observation readback",
                Some("audit row and drift/head readback"),
                error_codes::STORAGE_READ_FAILED,
                "read the latest baseline and retry drift audit with bounded scope",
            ),
        ],
    ),
    facade_contract(
        "verification",
        "VerificationOperation",
        "CF_KV verification/audit/v1 + verification/binding/v1 + bound Chrome tab readback",
        &[
            op(
                "inbox",
                true,
                true,
                "bound Chrome tab visible text + CF_KV verification/audit/v1 row",
                Some("masked audit row readback"),
                error_codes::ACTION_TARGET_INVALID,
                "bind/select the logged-in verification tab and inspect audit row",
            ),
            op(
                "poll",
                true,
                true,
                "bound Chrome tab visible text polling + CF_KV verification/audit/v1 row",
                Some("masked audit row readback from final poll"),
                error_codes::ACTION_TARGET_INVALID,
                "bind/select the logged-in verification tab and inspect audit row",
            ),
            op(
                "audit",
                false,
                false,
                "CF_KV verification/audit/v1 prefix scan",
                None,
                error_codes::STORAGE_READ_FAILED,
                "inspect verification audit prefix and storage health",
            ),
            op(
                "bind",
                true,
                true,
                "CF_KV verification/binding/v1 exact source row",
                Some("binding row readback"),
                error_codes::TOOL_PARAMS_INVALID,
                "provide a non-empty source and inspect binding row readback",
            ),
            op(
                "sources",
                false,
                false,
                "CF_KV verification/binding/v1 prefix scan",
                None,
                error_codes::STORAGE_READ_FAILED,
                "inspect verification binding prefix and storage health",
            ),
        ],
    ),
    facade_contract(
        "storage",
        "StorageOperation",
        "RocksDB CF metadata + exact row readbacks",
        &[
            op(
                "inspect",
                false,
                false,
                "RocksDB CF sizes/counts/samples",
                None,
                error_codes::STORAGE_READ_FAILED,
                "inspect storage health and CF metadata before mutating",
            ),
            op(
                "summary",
                false,
                false,
                "RocksDB CF live-data estimates + exact row counts",
                None,
                error_codes::STORAGE_READ_FAILED,
                "repair storage initialization and read CF metadata again",
            ),
            op(
                "put_probe_rows",
                true,
                false,
                "RocksDB CF probe rows",
                Some("CF row-count and size readback after probe row write"),
                error_codes::TOOL_PROFILE_POLICY_DENIED,
                "switch to an explicit maintenance profile before writing probe rows",
            ),
            op(
                "gc_once",
                true,
                false,
                "RocksDB CF row counts + audit retention report rows",
                Some("CF row-count and GC report readback after pass"),
                error_codes::TOOL_PROFILE_POLICY_DENIED,
                "switch to an explicit maintenance profile before running GC",
            ),
        ],
    ),
    facade_contract(
        "model",
        "ModelOperation",
        "local model registry CF_KV row + probe readback",
        &[
            op(
                "list",
                false,
                false,
                "CF_KV local model registry rows",
                None,
                error_codes::STORAGE_READ_FAILED,
                "read registry storage and probe diagnostics before routing a model",
            ),
            op(
                "status",
                false,
                false,
                "CF_KV local model registry rows + last probe fields",
                None,
                error_codes::STORAGE_READ_FAILED,
                "inspect registry rows and corrupt-row diagnostics",
            ),
            op(
                "probe",
                true,
                false,
                "live model endpoint + CF_KV probe evidence row",
                Some("registry row and probe-evidence readback"),
                error_codes::MODEL_TOOLS_UNSUPPORTED,
                "repair the real backend endpoint/socket/credentials and retry probe",
            ),
            op(
                "register",
                true,
                false,
                "CF_KV local model registry row",
                Some("registry row readback after forced structured probe"),
                error_codes::TOOL_PROFILE_POLICY_DENIED,
                "switch to an explicit maintenance profile before registering endpoints",
            ),
            op(
                "update",
                true,
                false,
                "CF_KV local model registry row",
                Some("forced structured tool-call probe row readback"),
                error_codes::TOOL_PROFILE_POLICY_DENIED,
                "switch to an explicit maintenance profile before mutating endpoints",
            ),
            op(
                "remove",
                true,
                false,
                "CF_KV local model registry row + secret row",
                Some("removed-row readback plus exact after-row absence check"),
                error_codes::TOOL_PROFILE_POLICY_DENIED,
                "switch to an explicit maintenance profile before removing endpoints",
            ),
        ],
    ),
    facade_contract(
        "cost",
        "CostOperation",
        "CF_AGENT_TRANSCRIPTS transcript rows + CF_KV cost/price/v1 rows",
        &[
            op(
                "summarize",
                false,
                false,
                "CF_AGENT_TRANSCRIPTS bounded transcript scan + CF_KV price rows",
                None,
                error_codes::STORAGE_READ_FAILED,
                "pass a spawn_id or bounded window, add missing price rows, or repair corrupt transcript/price rows",
            ),
            op(
                "price_list",
                false,
                false,
                "CF_KV cost/price/v1 rows",
                None,
                error_codes::STORAGE_READ_FAILED,
                "repair corrupt price rows and retry price_list",
            ),
            op(
                "price_put",
                true,
                false,
                "CF_KV cost/price/v1 row",
                Some("exact CF_KV price row readback after write"),
                error_codes::TOOL_PROFILE_POLICY_DENIED,
                "switch to an explicit maintenance profile before mutating prices",
            ),
            op(
                "price_delete",
                true,
                false,
                "CF_KV cost/price/v1 row",
                Some("exact CF_KV row absence/existed readback after delete"),
                error_codes::TOOL_PROFILE_POLICY_DENIED,
                "switch to an explicit maintenance profile before deleting prices",
            ),
        ],
    ),
    facade_contract(
        "hygiene",
        "HygieneOperation",
        "CF_KV hygiene flags + physical source rows + downstream learned-state joins",
        &[
            op(
                "scan_text",
                true,
                false,
                "caller text + optional physical source row",
                Some("CF_KV hygiene flag row readback when persist=true"),
                error_codes::TOOL_PROFILE_POLICY_DENIED,
                "run without persist for read-only scoring or switch to maintenance before persisting flags",
            ),
            op(
                "scan_storage",
                true,
                false,
                "CF_OBSERVATIONS/CF_TIMELINE source rows",
                Some("CF_KV hygiene flag rows linked to exact source keys"),
                error_codes::TOOL_PROFILE_POLICY_DENIED,
                "switch to an explicit maintenance profile before batch-scanning storage",
            ),
            op(
                "flags",
                false,
                false,
                "CF_KV hygiene/flag/v1 rows",
                None,
                error_codes::STORAGE_READ_FAILED,
                "inspect hygiene flag prefix/cursor and storage health",
            ),
            op(
                "report",
                false,
                false,
                "CF_KV hygiene flags + CF_EPISODES/CF_ROUTINES/profile-authoring joins",
                None,
                error_codes::STORAGE_READ_FAILED,
                "inspect hygiene report joins and storage health",
            ),
        ],
    ),
    facade_contract(
        "audit",
        "AuditOperation",
        "CF_ACTION_LOG + daemon lifecycle JSONL ledgers + profile audit storage rows",
        &[
            op(
                "command_query",
                false,
                false,
                "CF_ACTION_LOG bounded scan with row hashes and sanitized command metadata",
                None,
                error_codes::STORAGE_READ_FAILED,
                "narrow the time/tool/status filters or inspect CF_ACTION_LOG health",
            ),
            op(
                "lifecycle_events",
                false,
                false,
                "daemon-tool-events.jsonl sanitized tail read",
                None,
                error_codes::STORAGE_READ_FAILED,
                "inspect daemon lifecycle path/readability and retry a bounded tail query",
            ),
            op(
                "lifecycle_exits",
                false,
                false,
                "daemon-exit.jsonl sanitized tail read",
                None,
                error_codes::STORAGE_READ_FAILED,
                "inspect daemon lifecycle exit ledger and retry a bounded tail query",
            ),
            op(
                "profile_intelligence",
                false,
                false,
                "CF_ACTION_LOG + CF_EVENTS + CF_REFLEX_AUDIT + CF_SESSIONS profile-linked summaries",
                None,
                error_codes::STORAGE_READ_FAILED,
                "inspect profile id and audit CF health before retrying",
            ),
            op(
                "export_bundle",
                true,
                false,
                "CF_ACTION_LOG redacted export plus CF_KV audit_export consent row",
                Some("manifest/rows/redaction-report file hashes + consent row readback"),
                error_codes::AUDIT_EXPORT_CONSENT_REQUIRED,
                "provide explicit enabled strict consent and inspect output bundle hashes",
            ),
        ],
    ),
    facade_contract(
        "replay",
        "ReplayOperation",
        "Synapse replay JSONL artifacts + CF_KV demo-record row + CF_TIMELINE DemoMarker rows",
        &[
            op(
                "record",
                true,
                false,
                "live observations/events stream written to replay JSONL artifact",
                Some("replay JSONL file byte/hash/record inspection"),
                error_codes::STORAGE_READ_FAILED,
                "fix replay target/format/path and inspect the replay artifact root",
            ),
            op(
                "demo_status",
                false,
                false,
                "CF_KV timeline/demo-record/v1 hydrated DemoRecordControl",
                None,
                error_codes::STORAGE_READ_FAILED,
                "inspect CF_KV timeline/demo-record/v1 and retry demo_status",
            ),
            op(
                "demo_start",
                true,
                false,
                "CF_KV timeline/demo-record/v1 + CF_TIMELINE DemoMarker start row",
                Some("demo control row + command audit row readback"),
                error_codes::STORAGE_READ_FAILED,
                "fix profile/duration/path and inspect demo control/timeline rows",
            ),
            op(
                "demo_stop",
                true,
                false,
                "CF_KV timeline/demo-record/v1 + CF_TIMELINE DemoMarker rows + replay JSONL artifact",
                Some("replay JSONL file byte/hash/record inspection"),
                error_codes::STORAGE_READ_FAILED,
                "inspect active demo status and source rows before retrying demo_stop",
            ),
            op(
                "artifact_inspect",
                false,
                false,
                "replay JSONL artifact bytes under the Synapse replay root",
                None,
                error_codes::STORAGE_READ_FAILED,
                "verify the replay artifact path exists and contains valid JSONL",
            ),
        ],
    ),
    facade_contract(
        "privacy",
        "PrivacyOperation",
        "CF_KV timeline/control/v1 + CF_TIMELINE rows/audit rows + hygiene flag/taint rows",
        &[
            op(
                "pause",
                true,
                false,
                "CF_KV timeline/control/v1 + CF_TIMELINE boundary row",
                Some("timeline control row and boundary-row readback"),
                error_codes::TOOL_PROFILE_POLICY_DENIED,
                "escalate to an explicit privacy/admin profile and inspect timeline control rows",
            ),
            op(
                "resume",
                true,
                false,
                "CF_KV timeline/control/v1 + CF_TIMELINE boundary row",
                Some("timeline control row and boundary-row readback"),
                error_codes::TOOL_PROFILE_POLICY_DENIED,
                "escalate to an explicit privacy/admin profile and inspect timeline control rows",
            ),
            op(
                "exclusions",
                true,
                false,
                "CF_KV timeline/control/v1 runtime exclusions",
                Some("runtime/effective exclusion set readback"),
                error_codes::TOOL_PROFILE_POLICY_DENIED,
                "escalate before mutating exclusions; read-only exclusion inspection is allowed",
            ),
            op(
                "redact",
                true,
                false,
                "physical source rows + hygiene taint/audit rows",
                Some("redacted source rows plus hygiene taint/audit readback"),
                error_codes::TOOL_PROFILE_POLICY_DENIED,
                "escalate to an explicit privacy/admin profile and inspect source rows plus hygiene taint/audit rows",
            ),
            op(
                "purge",
                true,
                false,
                "CF_TIMELINE rows + purge audit row",
                Some("CF_TIMELINE row count/audit-key readback"),
                error_codes::TOOL_PROFILE_POLICY_DENIED,
                "escalate to an explicit privacy/admin profile and inspect CF_TIMELINE rows plus purge audit row",
            ),
        ],
    ),
    facade_contract(
        "setup",
        "SetupOperation",
        "host setup readback + daemon transport configuration",
        &[
            op(
                "status",
                false,
                false,
                "host setup files + daemon process/socket state",
                None,
                error_codes::TOOL_INTERNAL_ERROR,
                "repair the exact unreadable setup file/env prerequisite and retry",
            ),
            op(
                "doctor",
                false,
                false,
                "host setup files + daemon process/socket state",
                None,
                error_codes::TOOL_INTERNAL_ERROR,
                "repair the exact missing local prerequisite and read the configured SoT again",
            ),
            op(
                "repair",
                true,
                false,
                "host setup files + external setup script process/socket state",
                Some("post-repair daemon process/socket/token/config readback"),
                error_codes::TOOL_PROFILE_POLICY_DENIED,
                "switch to an explicit maintenance profile and run repair from an external process",
            ),
        ],
    ),
    facade_contract(
        "telemetry",
        "TelemetryOperation",
        "telemetry counters/events + public/implementation tool counts",
        &[op(
            "status",
            false,
            false,
            "telemetry counters/events + tool profile snapshot",
            None,
            error_codes::STORAGE_READ_FAILED,
            "read telemetry storage health and compare public/implementation count snapshots",
        )],
    ),
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FacadeToolContractSpec {
    tool_name: &'static str,
    operation_enum: &'static str,
    source_of_truth: &'static str,
    operations: &'static [FacadeOperationContractSpec],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FacadeOperationContractSpec {
    operation: &'static str,
    mutates_state: bool,
    target_required: bool,
    source_of_truth: &'static str,
    readback_source_of_truth: Option<&'static str>,
    error_code: &'static str,
    remediation: &'static str,
}

const fn facade_contract(
    tool_name: &'static str,
    operation_enum: &'static str,
    source_of_truth: &'static str,
    operations: &'static [FacadeOperationContractSpec],
) -> FacadeToolContractSpec {
    FacadeToolContractSpec {
        tool_name,
        operation_enum,
        source_of_truth,
        operations,
    }
}

const fn op(
    operation: &'static str,
    mutates_state: bool,
    target_required: bool,
    source_of_truth: &'static str,
    readback_source_of_truth: Option<&'static str>,
    error_code: &'static str,
    remediation: &'static str,
) -> FacadeOperationContractSpec {
    FacadeOperationContractSpec {
        operation,
        mutates_state,
        target_required,
        source_of_truth,
        readback_source_of_truth,
        error_code,
        remediation,
    }
}

const NORMAL_ALLOWED_EXACT: &[&str] = PUBLIC_TOOL_NAMES;
const NORMAL_ALLOWED_PREFIXES: &[&str] = &[];

const BROWSER_CONTROL_ALLOWED_EXACT: &[&str] = PUBLIC_TOOL_NAMES;

const BROWSER_DEBUGGER_ONLY_EXACT: &[&str] = &[
    "browser_add_init_script",
    "browser_add_script_tag",
    "browser_add_style_tag",
    "browser_console_messages",
    "browser_drag",
    "browser_drop",
    "browser_emulate",
    "browser_evaluate",
    "browser_expose_binding",
    "browser_handle_dialog",
    "browser_network",
    "browser_network_har",
    "browser_network_overrides",
    "browser_pdf",
    "browser_route",
];

const BROWSER_DEBUGGER_ALLOWED_EXACT: &[&str] = PUBLIC_TOOL_NAMES;

const BREAK_GLASS_HAZARDOUS_TOOLS: &[&str] = &[
    "act_click",
    "act_clipboard",
    "act_combo",
    "act_focus_window",
    "act_keymap",
    "act_pad",
    "act_press",
    "act_scroll",
    "act_set_field_text",
    "act_set_value",
    "act_stroke",
    "act_type",
    "hidden_desktop_pip_frame",
    "release_all",
];

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ToolProfileKind {
    NormalAgent,
    BrowserControl,
    /// Browser-only CDP / chrome.debugger capability lane. This keeps
    /// attach-capable browser tools explicit without exposing raw OS foreground,
    /// shell, or agent-spawn surfaces.
    BrowserDebugger,
    BreakGlass,
    /// Synapse-spawned local-model agent profile (gemma/DeepSeek/etc., #1031).
    /// It receives the same <=40 public facade surface as every other profile;
    /// broader authority is enforced inside the facades and action guards rather
    /// than by exposing raw implementation tools in discovery.
    FullCapability,
}

impl ToolProfileKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::NormalAgent => "normal_agent",
            Self::BrowserControl => "browser_control",
            Self::BrowserDebugger => "browser_debugger",
            Self::BreakGlass => "break_glass",
            Self::FullCapability => "full_capability",
        }
    }

    /// Whether this profile is permitted to reach the real human OS foreground
    /// input tier. Normal/task profiles still preserve foreground-equivalent
    /// capability through `agent_logical_foreground` / `foreground_lane` routes;
    /// only the serialized real-foreground tier needs break-glass/full-capability
    /// proof (#999/#1219).
    pub(crate) const fn allows_foreground_tier(self) -> bool {
        matches!(self, Self::BreakGlass | Self::FullCapability)
    }

    fn label(self) -> &'static str {
        match self {
            Self::NormalAgent => "normal_agent",
            Self::BrowserControl => "dashboard/browser-control task",
            Self::BrowserDebugger => "browser-debugger task",
            Self::BreakGlass => "break-glass/admin",
            Self::FullCapability => "full-capability local-model agent",
        }
    }

    fn is_visible(self, tool_name: &str) -> bool {
        match self {
            Self::BreakGlass | Self::FullCapability => PUBLIC_TOOL_NAMES.contains(&tool_name),
            Self::NormalAgent => {
                NORMAL_ALLOWED_EXACT.contains(&tool_name)
                    || NORMAL_ALLOWED_PREFIXES
                        .iter()
                        .any(|prefix| tool_name.starts_with(prefix))
            }
            Self::BrowserControl => BROWSER_CONTROL_ALLOWED_EXACT.contains(&tool_name),
            Self::BrowserDebugger => BROWSER_DEBUGGER_ALLOWED_EXACT.contains(&tool_name),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
struct PersistedToolProfile {
    schema_version: u32,
    row_kind: String,
    session_id: String,
    profile: ToolProfileKind,
    source: String,
    reason: Option<String>,
    set_by_session_id: Option<String>,
    stored_at_unix_ms: u64,
    allowed_tool_count: usize,
    allowed_tool_sha256: String,
    denied_break_glass_tools: Vec<String>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ToolProfileAssignment {
    pub schema_version: u32,
    pub row_kind: String,
    pub session_id: String,
    pub profile: ToolProfileKind,
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub set_by_session_id: Option<String>,
    pub stored_at_unix_ms: u64,
    pub allowed_tool_count: usize,
    pub allowed_tool_sha256: String,
    pub denied_break_glass_tools: Vec<String>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ToolProfileRowReadback {
    pub cf_name: &'static str,
    pub key_hex: String,
    pub value_len_bytes: u64,
    pub value_sha256: String,
    pub record: ToolProfileAssignment,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ToolProfileAuditReadback {
    pub cf_name: &'static str,
    pub key_hex: String,
    pub value_len_bytes: u64,
    pub value_sha256: String,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct PublicToolRegistrySnapshot {
    pub source_of_truth: &'static str,
    pub max_public_tool_count: usize,
    pub public_tool_count: usize,
    pub public_tool_sha256: String,
    pub public_tool_names: Vec<String>,
    pub implementation_tool_count: usize,
    pub registered_tools_present: Vec<String>,
    pub registered_tools_missing: Vec<String>,
    pub duplicate_public_tool_names: Vec<String>,
    pub forbidden_public_tool_names: Vec<String>,
    pub over_limit_by: usize,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct FacadeOperationContractSnapshot {
    pub operation: &'static str,
    pub mutates_state: bool,
    pub target_required: bool,
    pub source_of_truth: &'static str,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub readback_source_of_truth: Option<&'static str>,
    pub error_code: &'static str,
    pub remediation: &'static str,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct FacadeToolContractSnapshot {
    pub tool_name: &'static str,
    pub operation_enum: &'static str,
    pub source_of_truth: &'static str,
    pub operations: Vec<FacadeOperationContractSnapshot>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct FacadeContractSnapshot {
    pub source_of_truth: &'static str,
    pub structured_error_contract: &'static str,
    pub public_tool_count: usize,
    pub contract_tool_count: usize,
    pub operation_count: usize,
    pub mutating_operation_count: usize,
    pub facade_contract_sha256: String,
    pub contract_tool_names: Vec<String>,
    pub missing_contract_tool_names: Vec<String>,
    pub unknown_contract_tool_names: Vec<String>,
    pub duplicate_contract_tool_names: Vec<String>,
    pub duplicate_operation_names: Vec<String>,
    pub invalid_contract_reasons: Vec<String>,
    pub contracts: Vec<FacadeToolContractSnapshot>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ToolProfileSnapshot {
    pub source_of_truth: &'static str,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    pub profile: ToolProfileKind,
    pub profile_label: &'static str,
    pub source: String,
    pub implementation_tool_count: usize,
    pub visible_tool_count: usize,
    pub visible_tool_sha256: String,
    pub visible_tool_names: Vec<String>,
    pub denied_break_glass_tools: Vec<String>,
    pub foreground_capability: ToolProfileForegroundCapability,
    pub hidden_tool_routes: Vec<HiddenToolCapabilityRoute>,
    pub public_tool_registry: PublicToolRegistrySnapshot,
    pub facade_contract: FacadeContractSnapshot,
    pub codex_client_surface: CodexClientSurfaceSnapshot,
    /// #1352: this session's CURRENT readiness for the real OS-foreground route —
    /// whether it already holds the lease + a break_glass profile, and the exact
    /// remaining steps. Lets an agent preflight the foreground route before
    /// attempting a foreground-only action instead of discovering the gate by trial.
    pub foreground_route: ToolProfileForegroundRoute,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy_row: Option<ToolProfileRowReadback>,
}

#[derive(Clone, Copy, Debug, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CodexClientSurfaceStatus {
    HostSnapshotMatchesPublicTools,
    HostSnapshotMissing,
    HostSnapshotReadError,
    HostSnapshotMissingPublicTools,
    RestartRequiredForLiveCodexPid,
    RestartHandoffPresentForDeadPid,
    HandoffReadError,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct CodexToolSurfaceSnapshotReadback {
    pub path: String,
    pub exists: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub len_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_count: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_surface_sha256: Option<String>,
    pub tool_names: Vec<String>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct CodexRestartHandoffReadback {
    pub path: String,
    pub exists: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub len_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at_utc: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    pub required_restart: bool,
    pub no_in_process_hot_refresh: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stale_codex_pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stale_codex_command_line: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_issue_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daemon_pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daemon_bind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daemon_pid_role: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daemon_pid_authoritative_for_configured_bind: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daemon_tool_count: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daemon_tool_surface_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_process_start_snapshot_status: Option<String>,
    #[serde(skip)]
    #[schemars(skip)]
    pub current_process_start_env_hash: Option<String>,
    pub live_daemon_pid: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daemon_pid_matches_live_daemon: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daemon_pid_mismatch_detail: Option<String>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct CodexProcessReadback {
    pub source_of_truth: &'static str,
    pub pid: u32,
    pub parent_pid: Option<u32>,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exe: Option<String>,
    pub command_line: String,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct CodexClientSurfaceSnapshot {
    pub source_of_truth: &'static str,
    pub status: CodexClientSurfaceStatus,
    pub diagnostic_code: &'static str,
    pub remediation: &'static str,
    pub host_snapshot: CodexToolSurfaceSnapshotReadback,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_restart_handoff: Option<CodexRestartHandoffReadback>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub live_stale_codex_process: Option<CodexProcessReadback>,
    pub public_tools_missing_from_host_snapshot: Vec<String>,
    pub host_snapshot_tools_missing_from_public_registry: Vec<String>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ToolProfileForegroundRoute {
    /// True when this session currently owns the foreground input lease.
    pub holds_foreground_lease: bool,
    /// True when the current profile already exposes raw OS-foreground primitives.
    pub profile_allows_foreground: bool,
    /// True when a real OS-foreground action can run right now (lease held AND a
    /// foreground-capable profile) — no further escalation needed.
    pub foreground_route_ready: bool,
    /// The exact ordered steps still required to reach a runnable foreground
    /// action; empty when `foreground_route_ready`.
    pub remaining_steps: Vec<String>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ToolProfileForegroundCapability {
    pub source_of_truth: &'static str,
    pub profile_preserves_capability: bool,
    pub human_os_foreground: &'static str,
    pub agent_logical_foreground: &'static str,
    pub preferred_path: &'static str,
    pub real_os_foreground_path: &'static str,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct HiddenToolCapabilityRoute {
    pub hidden_tool: String,
    pub status: &'static str,
    pub preferred_tools: Vec<String>,
    pub agent_logical_foreground_policy: &'static str,
    pub human_os_foreground_policy: &'static str,
    pub break_glass_policy: &'static str,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ToolProfileStatusResponse {
    pub snapshot: ToolProfileSnapshot,
    /// #1559: effective M3 permission grants, their config source, and any
    /// active runtime reality-write overlay — so the effective write posture is
    /// coherent and `full_capability` is never mistaken for a write grant.
    pub m3_permissions: M3PermissionStatus,
}

/// #1559: coherent effective-permission readback.
#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct M3PermissionStatus {
    pub source_of_truth: &'static str,
    /// The M3 grant names currently in effect at startup (e.g. READ_STORAGE).
    pub effective_grant_names: Vec<String>,
    /// Where those startup grants came from (env / CLI / fail-closed default).
    pub config_source: &'static str,
    /// True when a non-expired runtime reality-write overlay is active.
    pub reality_write_overlay_active: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reality_write_overlay: Option<RealityWriteOverlayReadback>,
    pub profile_independent_of_grants_note: &'static str,
}

/// #1559: serialized readback of the active runtime reality-write overlay.
#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct RealityWriteOverlayReadback {
    pub granted_by: String,
    pub reason: String,
    pub granted_at: String,
    pub expires_at: String,
    pub remaining_ms: u64,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ToolProfileSetParams {
    pub profile: ToolProfileKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default)]
    #[schemars(default)]
    pub confirm_break_glass: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ProfileOperation {
    Status,
    Set,
    /// #1559: install the runtime reality-write opt-in overlay. Gated exactly
    /// like an explicit break_glass escalation (confirm_break_glass + non-empty
    /// reason + this session owns the foreground input lease).
    GrantRealityWrite,
    /// #1559: clear the runtime reality-write overlay, restoring the fail-closed
    /// default. De-escalation, so it needs only an MCP session id (audited).
    RevokeRealityWrite,
}

impl ProfileOperation {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Status => "status",
            Self::Set => "set",
            Self::GrantRealityWrite => "grant_reality_write",
            Self::RevokeRealityWrite => "revoke_reality_write",
        }
    }
}

const fn default_profile_operation() -> ProfileOperation {
    ProfileOperation::Status
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProfileParams {
    #[serde(default = "default_profile_operation")]
    #[schemars(default = "default_profile_operation")]
    pub operation: ProfileOperation,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<ToolProfileKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default)]
    #[schemars(default)]
    pub confirm_break_glass: bool,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProfileResponse {
    pub operation: ProfileOperation,
    pub source_of_truth: &'static str,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<ToolProfileStatusResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub set: Option<ToolProfileSetResponse>,
    /// #1559: grant/revoke result for the runtime reality-write overlay.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reality_write: Option<RealityWriteGrantResponse>,
}

/// #1559: result of a `profile operation=grant_reality_write` /
/// `revoke_reality_write` call. Carries the durable audit readbacks and the
/// overlay's bounded-TTL expiry so the caller can prove the escalation happened
/// and see exactly when it lapses.
#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct RealityWriteGrantResponse {
    pub operation: ProfileOperation,
    pub source_of_truth: &'static str,
    /// True when a runtime reality-write overlay is active after this call
    /// (always false after revoke).
    pub overlay_active: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub granted_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub granted_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remaining_ms: Option<u64>,
    pub ttl_seconds: u64,
    pub lease_proof: ToolProfileLeaseProof,
    pub intent_audit: ToolProfileAuditReadback,
    pub final_audit: ToolProfileAuditReadback,
    pub profile_independent_of_grants_note: &'static str,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ToolProfileLeaseProof {
    pub required: bool,
    pub held: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_session_id: Option<String>,
    pub caller_is_owner: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_in_ms: Option<u64>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ToolProfileSetResponse {
    pub before: ToolProfileSnapshot,
    pub after: ToolProfileSnapshot,
    pub row_readback: ToolProfileRowReadback,
    pub intent_audit: ToolProfileAuditReadback,
    pub final_audit: ToolProfileAuditReadback,
    pub lease_proof: ToolProfileLeaseProof,
}

#[tool_router(router = tool_profile_tool_router, vis = "pub(super)")]
impl SynapseService {
    #[tool(
        description = "Public profile facade. operation=status reads this MCP session's effective profile, visible public facade tools, durable CF_SESSIONS policy row, facade contract, and effective M3 permission grants (with config source + any active reality-write overlay). operation=set persists a new profile through the same audited readback path as tool_profile_set; explicit advanced profiles require confirm_break_glass=true and a non-empty reason, and break_glass/full_capability also require the foreground input lease. operation=grant_reality_write installs a bounded-TTL, audited, revocable reality-write opt-in overlay (gated exactly like break_glass: confirm_break_glass=true + non-empty reason + this session owns the foreground input lease); operation=revoke_reality_write clears it. The selected profile is INDEPENDENT of M3 permission grants and never yields WRITE_STORAGE by itself."
    )]
    pub async fn profile(
        &self,
        params: Parameters<ProfileParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<ProfileResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "profile",
            operation = params.0.operation.as_str(),
            "tool.invocation kind=profile"
        );
        let params = params.0;
        match params.operation {
            ProfileOperation::Status => Ok(Json(ProfileResponse {
                operation: ProfileOperation::Status,
                source_of_truth: TOOL_PROFILE_SOURCE_OF_TRUTH,
                status: Some(self.tool_profile_status_response(&request_context)?),
                set: None,
                reality_write: None,
            })),
            ProfileOperation::Set => {
                let profile = params.profile.ok_or_else(|| {
                    profile_facade_error(
                        ProfileOperation::Set,
                        "profile operation=set requires profile",
                        "pass profile=normal_agent, browser_control, browser_debugger, break_glass, or full_capability",
                    )
                })?;
                let set = self
                    .tool_profile_set_response(
                        ToolProfileSetParams {
                            profile,
                            reason: params.reason,
                            confirm_break_glass: params.confirm_break_glass,
                        },
                        &request_context,
                        "profile",
                        "set",
                    )
                    .await?;
                Ok(Json(ProfileResponse {
                    operation: ProfileOperation::Set,
                    source_of_truth: TOOL_PROFILE_SOURCE_OF_TRUTH,
                    status: None,
                    set: Some(set),
                    reality_write: None,
                }))
            }
            ProfileOperation::GrantRealityWrite => {
                let session_id = super::context::mcp_session_id_from_request_context(
                    &request_context,
                )?
                .ok_or_else(|| {
                    mcp_error(
                        error_codes::HTTP_SESSION_INVALID,
                        "profile operation=grant_reality_write requires an MCP session id so the escalation can be persisted",
                    )
                })?;
                let reality_write = self.apply_reality_write_grant(
                    &session_id,
                    params.reason.as_deref(),
                    params.confirm_break_glass,
                )?;
                Ok(Json(ProfileResponse {
                    operation: ProfileOperation::GrantRealityWrite,
                    source_of_truth: REALITY_WRITE_GRANT_SOURCE_OF_TRUTH,
                    status: None,
                    set: None,
                    reality_write: Some(reality_write),
                }))
            }
            ProfileOperation::RevokeRealityWrite => {
                let session_id = super::context::mcp_session_id_from_request_context(
                    &request_context,
                )?
                .ok_or_else(|| {
                    mcp_error(
                        error_codes::HTTP_SESSION_INVALID,
                        "profile operation=revoke_reality_write requires an MCP session id so the revoke can be persisted",
                    )
                })?;
                let reality_write = self.apply_reality_write_revoke(&session_id)?;
                Ok(Json(ProfileResponse {
                    operation: ProfileOperation::RevokeRealityWrite,
                    source_of_truth: REALITY_WRITE_GRANT_SOURCE_OF_TRUTH,
                    status: None,
                    set: None,
                    reality_write: Some(reality_write),
                }))
            }
        }
    }

    #[tool(
        description = "Read this MCP session's effective tool profile, visible tools/list names, durable CF_SESSIONS policy row, and capability-preserving routes for hidden raw foreground/browser-debugger primitives. The readback distinguishes human_os_foreground from agent_logical_foreground and the browser debugger lane: normal_agent/browser_control expose debugger-free already-open Chrome routes, browser_debugger explicitly exposes raw-CDP/chrome.debugger browser tools, and real OS foreground primitives stay reachable only through lease + break_glass.",
        input_schema = empty_input_schema()
    )]
    pub async fn tool_profile_status(
        &self,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<ToolProfileStatusResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "tool_profile_status",
            "tool.invocation kind=tool_profile_status"
        );
        self.tool_profile_status_response(&request_context)
            .map(Json)
    }

    #[tool(
        description = "Set this MCP session's durable tool profile. Every profile keeps discovery on the <=40 public facade surface. browser_debugger, break_glass, and full_capability enable stricter facade operations only when confirm_break_glass=true and reason is non-empty; break_glass/full_capability also require this session to own the foreground input lease."
    )]
    pub async fn tool_profile_set(
        &self,
        params: Parameters<ToolProfileSetParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<ToolProfileSetResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "tool_profile_set",
            "tool.invocation kind=tool_profile_set"
        );
        self.tool_profile_set_response(
            params.0,
            &request_context,
            "tool_profile_set",
            "profile_set",
        )
        .await
        .map(Json)
    }
}

impl SynapseService {
    fn tool_profile_status_response(
        &self,
        request_context: &RequestContext<RoleServer>,
    ) -> Result<ToolProfileStatusResponse, ErrorData> {
        let session_id = super::context::mcp_session_id_from_request_context(request_context)?;
        Ok(ToolProfileStatusResponse {
            snapshot: self.tool_profile_snapshot(session_id.as_deref())?,
            m3_permissions: self.m3_permission_status()?,
        })
    }

    /// #1559: coherent effective-permission readback drawn from live M3State —
    /// startup grants + config source + any active runtime reality-write overlay.
    pub(super) fn m3_permission_status(&self) -> Result<M3PermissionStatus, ErrorData> {
        let mut state = self.m3_state.lock().map_err(|_err| {
            mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                "M3 service state lock poisoned while reading permission status",
            )
        })?;
        // Consult (and lazily clear any expired) overlay so the readback matches
        // exactly what enforcement would decide right now.
        let overlay_active = state.reality_write_grant_active();
        let overlay = state.reality_write_grant_snapshot().map(|snapshot| {
            RealityWriteOverlayReadback {
                granted_by: snapshot.granted_by,
                reason: snapshot.reason,
                granted_at: snapshot.granted_at.to_rfc3339(),
                expires_at: snapshot.expires_at.to_rfc3339(),
                remaining_ms: snapshot.remaining_ms,
            }
        });
        let effective_grant_names = state
            .permission_grants
            .names()
            .iter()
            .map(|name| (*name).to_owned())
            .collect();
        let config_source = state.permission_grants_source;
        drop(state);
        Ok(M3PermissionStatus {
            source_of_truth:
                "M3State.permission_grants (startup) + in-memory reality_write_grant overlay",
            effective_grant_names,
            config_source,
            reality_write_overlay_active: overlay_active,
            reality_write_overlay: overlay,
            profile_independent_of_grants_note: PROFILE_INDEPENDENT_OF_GRANTS_NOTE,
        })
    }

    /// #1559: install the runtime reality-write opt-in overlay for a session.
    /// Gated exactly like an explicit break_glass escalation (confirm + non-empty
    /// reason + this session owns the foreground input lease) and audited to
    /// CF_ACTION_LOG (intent + final). Fail-closed: any unmet gate denies and the
    /// overlay is never installed. Takes `session_id` directly so the facade
    /// handler and unit tests share one gated path.
    pub(super) fn apply_reality_write_grant(
        &self,
        session_id: &str,
        reason: Option<&str>,
        confirm_break_glass: bool,
    ) -> Result<RealityWriteGrantResponse, ErrorData> {
        let reason = normalize_reason(reason)?;
        let lease_proof = break_glass_lease_proof(session_id, ToolProfileKind::BreakGlass);
        let command_payload = json!({
            "operation": "grant_reality_write",
            "reason": reason,
            "confirm_break_glass": confirm_break_glass,
        });
        let command_before = json!({
            "source_of_truth": REALITY_WRITE_GRANT_SOURCE_OF_TRUTH,
            "lease_proof": lease_proof,
        });
        let intent_audit = audit_readback(self.command_audit_intent(
            super::command_audit::CommandAuditInput::mcp(
                "profile",
                "grant_reality_write",
                Some(session_id.to_owned()),
                Some(session_id.to_owned()),
                command_payload.clone(),
                command_before.clone(),
                Value::Null,
                "pending",
            ),
        )?);

        if let Err(error) =
            validate_reality_write_grant_gate(session_id, reason.as_deref(), confirm_break_glass, &lease_proof)
        {
            let final_audit = self.command_audit_final(
                super::command_audit::CommandAuditInput::mcp(
                    "profile",
                    "grant_reality_write",
                    Some(session_id.to_owned()),
                    Some(session_id.to_owned()),
                    command_payload,
                    command_before,
                    json!({
                        "source_of_truth": REALITY_WRITE_GRANT_SOURCE_OF_TRUTH,
                        "overlay_active": false,
                        "lease_proof": lease_proof,
                    }),
                    "error",
                )
                .with_error(super::command_audit::command_audit_error_from_error_data(&error)),
            )?;
            let _final_audit = audit_readback(final_audit);
            return Err(error);
        }

        let snapshot = {
            let mut state = self.m3_state.lock().map_err(|_err| {
                mcp_error(
                    error_codes::TOOL_INTERNAL_ERROR,
                    "M3 service state lock poisoned while granting reality-write overlay",
                )
            })?;
            state.grant_reality_write(session_id.to_owned(), reason.clone().unwrap_or_default())
        };
        tracing::warn!(
            code = "REALITY_WRITE_GRANTED",
            session_id = %session_id,
            reason = reason.as_deref().unwrap_or(""),
            expires_at = %snapshot.expires_at.to_rfc3339(),
            ttl_seconds = crate::m3::REALITY_WRITE_GRANT_MAX_TTL.as_secs(),
            "runtime reality-write overlay granted (#1559)"
        );
        let final_audit = audit_readback(self.command_audit_final(
            super::command_audit::CommandAuditInput::mcp(
                "profile",
                "grant_reality_write",
                Some(session_id.to_owned()),
                Some(session_id.to_owned()),
                command_payload,
                command_before,
                json!({
                    "source_of_truth": REALITY_WRITE_GRANT_SOURCE_OF_TRUTH,
                    "overlay_active": true,
                    "granted_by": session_id,
                    "expires_at": snapshot.expires_at.to_rfc3339(),
                    "remaining_ms": snapshot.remaining_ms,
                    "lease_proof": lease_proof,
                }),
                "ok",
            ),
        )?);

        Ok(RealityWriteGrantResponse {
            operation: ProfileOperation::GrantRealityWrite,
            source_of_truth: REALITY_WRITE_GRANT_SOURCE_OF_TRUTH,
            overlay_active: true,
            granted_by: Some(session_id.to_owned()),
            reason,
            granted_at: Some(snapshot.granted_at.to_rfc3339()),
            expires_at: Some(snapshot.expires_at.to_rfc3339()),
            remaining_ms: Some(snapshot.remaining_ms),
            ttl_seconds: crate::m3::REALITY_WRITE_GRANT_MAX_TTL.as_secs(),
            lease_proof,
            intent_audit,
            final_audit,
            profile_independent_of_grants_note: PROFILE_INDEPENDENT_OF_GRANTS_NOTE,
        })
    }

    /// #1559: clear the runtime reality-write overlay for a session, restoring
    /// the fail-closed default with no residue. De-escalation, so it needs only
    /// an MCP session id; still audited to CF_ACTION_LOG.
    pub(super) fn apply_reality_write_revoke(
        &self,
        session_id: &str,
    ) -> Result<RealityWriteGrantResponse, ErrorData> {
        let lease_proof = break_glass_lease_proof(session_id, ToolProfileKind::BreakGlass);
        let command_payload = json!({ "operation": "revoke_reality_write" });
        let command_before = json!({ "source_of_truth": REALITY_WRITE_GRANT_SOURCE_OF_TRUTH });
        let intent_audit = audit_readback(self.command_audit_intent(
            super::command_audit::CommandAuditInput::mcp(
                "profile",
                "revoke_reality_write",
                Some(session_id.to_owned()),
                Some(session_id.to_owned()),
                command_payload.clone(),
                command_before.clone(),
                Value::Null,
                "pending",
            ),
        )?);

        let revoked = {
            let mut state = self.m3_state.lock().map_err(|_err| {
                mcp_error(
                    error_codes::TOOL_INTERNAL_ERROR,
                    "M3 service state lock poisoned while revoking reality-write overlay",
                )
            })?;
            state.revoke_reality_write()
        };
        tracing::warn!(
            code = "REALITY_WRITE_REVOKED",
            session_id = %session_id,
            was_active = revoked.is_some(),
            "runtime reality-write overlay revoked (#1559)"
        );
        let final_audit = audit_readback(self.command_audit_final(
            super::command_audit::CommandAuditInput::mcp(
                "profile",
                "revoke_reality_write",
                Some(session_id.to_owned()),
                Some(session_id.to_owned()),
                command_payload,
                command_before,
                json!({
                    "source_of_truth": REALITY_WRITE_GRANT_SOURCE_OF_TRUTH,
                    "overlay_active": false,
                    "was_active": revoked.is_some(),
                }),
                "ok",
            ),
        )?);

        Ok(RealityWriteGrantResponse {
            operation: ProfileOperation::RevokeRealityWrite,
            source_of_truth: REALITY_WRITE_GRANT_SOURCE_OF_TRUTH,
            overlay_active: false,
            granted_by: revoked.as_ref().map(|snapshot| snapshot.granted_by.clone()),
            reason: revoked.as_ref().map(|snapshot| snapshot.reason.clone()),
            granted_at: None,
            expires_at: None,
            remaining_ms: None,
            ttl_seconds: 0,
            lease_proof,
            intent_audit,
            final_audit,
            profile_independent_of_grants_note: PROFILE_INDEPENDENT_OF_GRANTS_NOTE,
        })
    }

    async fn tool_profile_set_response(
        &self,
        params: ToolProfileSetParams,
        request_context: &RequestContext<RoleServer>,
        audit_tool: &'static str,
        audit_verb: &'static str,
    ) -> Result<ToolProfileSetResponse, ErrorData> {
        let session_id = super::context::mcp_session_id_from_request_context(request_context)?
            .ok_or_else(|| {
                mcp_error(
                    error_codes::HTTP_SESSION_INVALID,
                    format!(
                        "{audit_tool} operation={audit_verb} requires an MCP session id so the policy decision can be persisted"
                    ),
                )
            })?;
        let reason = normalize_reason(params.reason.as_deref())?;
        let before = self.tool_profile_snapshot(Some(&session_id))?;
        let lease_proof = break_glass_lease_proof(&session_id, params.profile);
        let command_payload = json!({
            "requested_profile": params.profile.as_str(),
            "reason": reason,
            "confirm_break_glass": params.confirm_break_glass,
            "operation": audit_verb,
        });
        let command_before = json!({
            "source_of_truth": TOOL_PROFILE_SOURCE_OF_TRUTH,
            "before_profile": before.profile.as_str(),
            "before_visible_tool_count": before.visible_tool_count,
            "lease_proof": lease_proof,
        });
        let intent_audit = audit_readback(self.command_audit_intent(
            super::command_audit::CommandAuditInput::mcp(
                audit_tool,
                audit_verb,
                Some(session_id.clone()),
                Some(session_id.clone()),
                command_payload.clone(),
                command_before.clone(),
                Value::Null,
                "pending",
            ),
        )?);

        if let Err(error) = validate_profile_set_policy(
            &session_id,
            params.profile,
            reason.as_deref(),
            params.confirm_break_glass,
            &lease_proof,
        ) {
            let final_audit = self.command_audit_final(
                super::command_audit::CommandAuditInput::mcp(
                    audit_tool,
                    audit_verb,
                    Some(session_id.clone()),
                    Some(session_id.clone()),
                    command_payload,
                    command_before,
                    json!({
                        "source_of_truth": TOOL_PROFILE_SOURCE_OF_TRUTH,
                        "after_profile": before.profile.as_str(),
                        "lease_proof": lease_proof,
                    }),
                    "error",
                )
                .with_error(
                    super::command_audit::command_audit_error_from_error_data(&error),
                ),
            )?;
            let _final_audit = audit_readback(final_audit);
            return Err(error);
        }

        let row_readback = match self.write_tool_profile_assignment(
            &session_id,
            params.profile,
            audit_tool,
            reason.clone(),
            Some(session_id.clone()),
        ) {
            Ok(row) => row,
            Err(error) => {
                let final_audit = self.command_audit_final(
                    super::command_audit::CommandAuditInput::mcp(
                        audit_tool,
                        audit_verb,
                        Some(session_id.clone()),
                        Some(session_id.clone()),
                        command_payload,
                        command_before,
                        json!({
                            "source_of_truth": TOOL_PROFILE_SOURCE_OF_TRUTH,
                            "after_profile": before.profile.as_str(),
                            "lease_proof": lease_proof,
                        }),
                        "error",
                    )
                    .with_error(
                        super::command_audit::command_audit_error_from_error_data(&error),
                    ),
                )?;
                let _final_audit = audit_readback(final_audit);
                return Err(error);
            }
        };
        let after = self.tool_profile_snapshot(Some(&session_id))?;

        // #1020: the visible tool surface for this MCP session just changed.
        // Push a `notifications/tools/list_changed` so the client refetches
        // `tools/list` within the same session. The CF_SESSIONS row is already
        // the durable source of truth, so notification failure is logged loudly
        // and returned only through the daemon logs.
        if before.visible_tool_sha256 != after.visible_tool_sha256 {
            match request_context.peer.notify_tool_list_changed().await {
                Ok(()) => {
                    tracing::info!(
                        code = "MCP_TOOL_LIST_CHANGED_NOTIFIED",
                        session_id = %session_id,
                        tool = audit_tool,
                        operation = audit_verb,
                        before_profile = before.profile.as_str(),
                        after_profile = after.profile.as_str(),
                        before_visible_tool_count = before.visible_tool_count,
                        after_visible_tool_count = after.visible_tool_count,
                        "profile tool pushed notifications/tools/list_changed after a visible tool-surface change"
                    );
                }
                Err(notify_err) => {
                    tracing::error!(
                        code = "MCP_TOOL_LIST_CHANGED_NOTIFY_FAILED",
                        session_id = %session_id,
                        tool = audit_tool,
                        operation = audit_verb,
                        before_profile = before.profile.as_str(),
                        after_profile = after.profile.as_str(),
                        error = %notify_err,
                        "profile tool persisted the new profile but failed to push notifications/tools/list_changed; the client may need to reconnect to observe the updated tool surface"
                    );
                }
            }
        }

        let final_audit = audit_readback(self.command_audit_final(
            super::command_audit::CommandAuditInput::mcp(
                audit_tool,
                audit_verb,
                Some(session_id.clone()),
                Some(session_id),
                command_payload,
                command_before,
                json!({
                    "source_of_truth": TOOL_PROFILE_SOURCE_OF_TRUTH,
                    "after_profile": after.profile.as_str(),
                    "after_visible_tool_count": after.visible_tool_count,
                    "row_readback": row_readback,
                    "lease_proof": lease_proof,
                }),
                "ok",
            ),
        )?);

        Ok(ToolProfileSetResponse {
            before,
            after,
            row_readback,
            intent_audit,
            final_audit,
            lease_proof,
        })
    }

    pub(crate) fn tools_for_session_profile(
        &self,
        session_id: Option<&str>,
    ) -> Result<Vec<Tool>, ErrorData> {
        let snapshot = self.tool_profile_snapshot(session_id)?;
        let mut tools = self.full_sanitized_tools();
        if session_id.is_some() {
            tools.retain(|tool| snapshot.profile.is_visible(tool.name.as_ref()));
        }
        sort_tools_for_profile(&mut tools, snapshot.profile);
        Ok(tools)
    }

    pub(crate) fn tool_profile_snapshot(
        &self,
        session_id: Option<&str>,
    ) -> Result<ToolProfileSnapshot, ErrorData> {
        let full_tool_names = self.full_tool_names();
        let implementation_tool_count = full_tool_names.len();
        let public_tool_registry = public_tool_registry_snapshot_for(&full_tool_names)?;
        let facade_contract =
            facade_contract_snapshot_for(&public_tool_registry.public_tool_names)?;
        let (profile, source, policy_row) = match session_id {
            Some(session_id) => {
                let row = self.ensure_tool_profile_assignment(session_id)?;
                (row.record.profile, row.record.source.clone(), Some(row))
            }
            None => (
                ToolProfileKind::BreakGlass,
                "unscoped_stdio_admin".to_owned(),
                None,
            ),
        };
        let visible_tool_names = if session_id.is_some() {
            visible_tool_names_for_profile(profile, &full_tool_names)
        } else {
            full_tool_names
        };
        let visible_tool_sha256 = sha256_json_hex(&visible_tool_names)?;
        let denied_break_glass_tools = denied_break_glass_tools(&visible_tool_names);
        let hidden_tool_routes = hidden_tool_capability_routes(&visible_tool_names);
        let codex_client_surface =
            codex_client_surface_snapshot(&public_tool_registry.public_tool_names);
        Ok(ToolProfileSnapshot {
            source_of_truth: TOOL_PROFILE_SOURCE_OF_TRUTH,
            session_id: session_id.map(ToOwned::to_owned),
            profile,
            profile_label: profile.label(),
            source,
            implementation_tool_count,
            visible_tool_count: visible_tool_names.len(),
            visible_tool_sha256,
            visible_tool_names,
            denied_break_glass_tools,
            foreground_capability: foreground_capability_policy(profile),
            hidden_tool_routes,
            public_tool_registry,
            facade_contract,
            codex_client_surface,
            foreground_route: foreground_route_readiness(session_id, profile),
            policy_row,
        })
    }

    pub(crate) fn public_tool_registry_snapshot(
        &self,
    ) -> Result<PublicToolRegistrySnapshot, ErrorData> {
        public_tool_registry_snapshot_for(&self.full_tool_names())
    }

    pub(crate) fn facade_contract_snapshot() -> Result<FacadeContractSnapshot, ErrorData> {
        facade_contract_snapshot_for(&public_tool_names())
    }

    pub(crate) fn admit_tool_call_for_profile(
        &self,
        tool_name: &str,
        session_id: Option<&str>,
    ) -> Result<(), ErrorData> {
        let Some(session_id) = session_id else {
            return Ok(());
        };
        let full_tool_names = self.full_tool_names();
        if !full_tool_names.iter().any(|name| name == tool_name) {
            return Ok(());
        }
        let row = self.ensure_tool_profile_assignment(session_id)?;
        if row.record.profile.is_visible(tool_name) {
            return Ok(());
        }
        let visible_tool_names =
            visible_tool_names_for_profile(row.record.profile, &full_tool_names);
        let capability_route = hidden_tool_capability_route(tool_name);
        let error = ErrorData::new(
            ErrorCode(-32099),
            format!(
                "tool {tool_name:?} is hidden by MCP tool profile {} for session {session_id}",
                row.record.profile.as_str()
            ),
            Some(json!({
                "code": error_codes::TOOL_PROFILE_POLICY_DENIED,
                "tool": tool_name,
                "session_id": session_id,
                "profile": row.record.profile.as_str(),
                "profile_label": row.record.profile.label(),
                "source_of_truth": TOOL_PROFILE_SOURCE_OF_TRUTH,
                "policy_row": row,
                "visible_tool_count": visible_tool_names.len(),
                "capability_route": capability_route,
                "resolution": "use the named capability_route preferred tools for default agent work; call profile operation=set profile=browser_debugger with confirm_break_glass=true plus a non-empty reason for browser raw-CDP/chrome.debugger instrumentation; acquire the foreground input lease and call profile operation=set profile=break_glass with confirm_break_glass=true plus a non-empty reason for real human OS foreground work",
            })),
        );
        let command_payload = json!({
            "requested_tool": tool_name,
            "profile": row.record.profile.as_str(),
        });
        let command_before = json!({
            "source_of_truth": TOOL_PROFILE_SOURCE_OF_TRUTH,
            "policy_row": row,
            "visible_tool_count": visible_tool_names.len(),
        });
        self.command_audit_final(
            super::command_audit::CommandAuditInput::mcp(
                "tool_profile_policy",
                "tool_call_denied",
                Some(session_id.to_owned()),
                Some(session_id.to_owned()),
                command_payload,
                command_before,
                json!({
                    "source_of_truth": "CF_ACTION_LOG command_audit row",
                    "denied_tool": tool_name,
                    "capability_route": hidden_tool_capability_route(tool_name),
                }),
                "error",
            )
            .with_error(super::command_audit::command_audit_error_from_error_data(
                &error,
            )),
        )?;
        Err(error)
    }

    fn ensure_tool_profile_assignment(
        &self,
        session_id: &str,
    ) -> Result<ToolProfileRowReadback, ErrorData> {
        let is_local_agent = self.session_is_local_model_agent(session_id);
        match self.read_tool_profile_assignment(session_id)? {
            Some(row) => {
                // Self-heal: if a session was first seen before its MCP
                // initialize client identity landed in the registry, it may have
                // been written the least-privilege `default_normal_agent` row.
                // Once the registry classifies it as the trusted local-model
                // harness, upgrade it to the full-capability policy so the
                // local model is never left without facade-routed input
                // capability (#1031).
                // Only the *default* normal-agent row self-heals; an explicit
                // operator profile choice is never silently widened.
                if is_local_agent && row.record.source == "default_normal_agent" {
                    return self.write_tool_profile_assignment(
                        session_id,
                        ToolProfileKind::FullCapability,
                        "default_full_capability_local_agent",
                        None,
                        None,
                    );
                }
                if !self.tool_profile_assignment_surface_is_current(&row.record)? {
                    return self.write_tool_profile_assignment(
                        session_id,
                        row.record.profile,
                        row.record.source.clone(),
                        row.record.reason.clone(),
                        row.record.set_by_session_id,
                    );
                }
                Ok(row)
            }
            None => {
                let (profile, source) = if is_local_agent {
                    (
                        ToolProfileKind::FullCapability,
                        "default_full_capability_local_agent",
                    )
                } else {
                    (ToolProfileKind::NormalAgent, "default_normal_agent")
                };
                self.write_tool_profile_assignment(session_id, profile, source, None, None)
            }
        }
    }

    fn tool_profile_assignment_surface_is_current(
        &self,
        record: &ToolProfileAssignment,
    ) -> Result<bool, ErrorData> {
        let full_tool_names = self.full_tool_names();
        let allowed_tool_names = visible_tool_names_for_profile(record.profile, &full_tool_names);
        Ok(record.allowed_tool_count == allowed_tool_names.len()
            && record.allowed_tool_sha256 == sha256_json_hex(&allowed_tool_names)?
            && record.denied_break_glass_tools == denied_break_glass_tools(&allowed_tool_names))
    }

    /// True when `session_id` belongs to a Synapse-spawned local-model agent
    /// (the `synapse-local-model-agent` MCP client, classified `"local-model"`
    /// by [`super::session_registry::infer_agent_kind`]). Trust basis: Synapse is
    /// loopback-only + bearer-token + single-user, so the MCP client identity is
    /// a sound affordance signal here (it is NOT a cross-tenant security boundary;
    /// `clientInfo` is self-reported per MCP). Unknown / unclassified sessions
    /// return false and get the least-privilege default.
    fn session_is_local_model_agent(&self, session_id: &str) -> bool {
        self.session_registry_ref()
            .lock()
            .ok()
            .and_then(|registry| registry.agent_kind_for(session_id))
            .as_deref()
            == Some("local-model")
    }

    fn read_tool_profile_assignment(
        &self,
        session_id: &str,
    ) -> Result<Option<ToolProfileRowReadback>, ErrorData> {
        let db = self.m3_storage()?;
        let key = tool_profile_key(session_id);
        let rows = db
            .scan_cf_prefix(cf::CF_SESSIONS, &key)
            .map_err(|error| mcp_error(error.code(), error.to_string()))?;
        let Some((read_key, value)) = rows.into_iter().find(|(row_key, _)| row_key == &key) else {
            return Ok(None);
        };
        let persisted =
            synapse_storage::decode_json::<PersistedToolProfile>(&value).map_err(|error| {
                mcp_error(
                    error.code(),
                    format!("decode tool profile row failed for {session_id}: {error}"),
                )
            })?;
        if persisted.schema_version != TOOL_PROFILE_SCHEMA_VERSION
            || persisted.row_kind != TOOL_PROFILE_ROW_KIND
            || persisted.session_id != session_id
        {
            return Err(mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!(
                    "tool profile row mismatch for {session_id}: schema_version={} row_kind={} row_session_id={}",
                    persisted.schema_version, persisted.row_kind, persisted.session_id
                ),
            ));
        }
        let record = ToolProfileAssignment {
            schema_version: persisted.schema_version,
            row_kind: persisted.row_kind,
            session_id: persisted.session_id,
            profile: persisted.profile,
            source: persisted.source,
            reason: persisted.reason,
            set_by_session_id: persisted.set_by_session_id,
            stored_at_unix_ms: persisted.stored_at_unix_ms,
            allowed_tool_count: persisted.allowed_tool_count,
            allowed_tool_sha256: persisted.allowed_tool_sha256,
            denied_break_glass_tools: persisted.denied_break_glass_tools,
        };
        Ok(Some(ToolProfileRowReadback {
            cf_name: cf::CF_SESSIONS,
            key_hex: hex_lower(&read_key),
            value_len_bytes: value.len() as u64,
            value_sha256: sha256_hex(&value),
            record,
        }))
    }

    fn write_tool_profile_assignment(
        &self,
        session_id: &str,
        profile: ToolProfileKind,
        source: impl Into<String>,
        reason: Option<String>,
        set_by_session_id: Option<String>,
    ) -> Result<ToolProfileRowReadback, ErrorData> {
        let full_tool_names = self.full_tool_names();
        let allowed_tool_names = visible_tool_names_for_profile(profile, &full_tool_names);
        let allowed_tool_sha256 = sha256_json_hex(&allowed_tool_names)?;
        let record = ToolProfileAssignment {
            schema_version: TOOL_PROFILE_SCHEMA_VERSION,
            row_kind: TOOL_PROFILE_ROW_KIND.to_owned(),
            session_id: session_id.to_owned(),
            profile,
            source: source.into(),
            reason,
            set_by_session_id,
            stored_at_unix_ms: unix_ms_now(),
            allowed_tool_count: allowed_tool_names.len(),
            allowed_tool_sha256,
            denied_break_glass_tools: denied_break_glass_tools(&allowed_tool_names),
        };
        let encoded = synapse_storage::encode_json(&record).map_err(|error| {
            mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                format!("encode tool profile row failed for {session_id}: {error}"),
            )
        })?;
        let db = self.m3_storage()?;
        let key = tool_profile_key(session_id);
        db.put_batch_pressure_bypass(cf::CF_SESSIONS, [(key, encoded.clone())])
            .map_err(|error| mcp_error(error.code(), error.to_string()))?;
        let readback = self
            .read_tool_profile_assignment(session_id)?
            .ok_or_else(|| {
                mcp_error(
                    error_codes::STORAGE_CORRUPTED,
                    format!("tool profile row missing after write for {session_id}"),
                )
            })?;
        if readback.value_sha256 != sha256_hex(&encoded) {
            return Err(mcp_error(
                error_codes::STORAGE_CORRUPTED,
                format!("tool profile row readback hash mismatch for {session_id}"),
            ));
        }
        tracing::info!(
            code = "MCP_TOOL_PROFILE_PERSISTED",
            session_id,
            profile = profile.as_str(),
            visible_tool_count = readback.record.allowed_tool_count,
            key_hex = %readback.key_hex,
            "persisted MCP tool profile to CF_SESSIONS"
        );
        Ok(readback)
    }

    pub(crate) fn full_sanitized_tools(&self) -> Vec<Tool> {
        super::schema_sanitize::sanitize_tools(self.tool_router.list_all())
    }

    fn full_tool_names(&self) -> Vec<String> {
        let mut names = self
            .full_sanitized_tools()
            .into_iter()
            .map(|tool| tool.name.to_string())
            .collect::<Vec<_>>();
        names.sort();
        names
    }
}

fn validate_profile_set_policy(
    session_id: &str,
    profile: ToolProfileKind,
    reason: Option<&str>,
    confirm_break_glass: bool,
    lease_proof: &ToolProfileLeaseProof,
) -> Result<(), ErrorData> {
    if !matches!(
        profile,
        ToolProfileKind::BrowserDebugger
            | ToolProfileKind::BreakGlass
            | ToolProfileKind::FullCapability
    ) {
        return Ok(());
    }
    let profile_label = profile.as_str();
    if !confirm_break_glass {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("explicit profile={profile_label} requires confirm_break_glass=true"),
        ));
    }
    if reason.is_none_or(str::is_empty) {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("explicit profile={profile_label} requires a non-empty reason"),
        ));
    }
    if profile == ToolProfileKind::BrowserDebugger {
        return Ok(());
    }
    // The break_glass and full_capability policy profiles, when requested
    // *explicitly* via tool_profile_set, require foreground-lease proof. This
    // stops any agent from self-escalating to foreground-capable facade
    // operations by hand. The frictionless path to full_capability is the
    // automatic, client-identity-keyed default for the trusted local-model
    // harness (see `ensure_tool_profile_assignment`), never this tool.
    if !lease_proof.caller_is_owner {
        return Err(ErrorData::new(
            ErrorCode(-32099),
            format!(
                "explicit profile={profile_label} requires this MCP session to own the foreground input lease; current owner={:?}",
                lease_proof.owner_session_id
            ),
            Some(json!({
                "code": error_codes::ACTION_FOREGROUND_LEASE_NOT_HELD,
                "session_id": session_id,
                "profile": profile.as_str(),
                "lease_proof": lease_proof,
                "resolution": "call act operation=lease_acquire first, then retry profile operation=set or tool_profile_set with confirm_break_glass=true and a reason",
            })),
        ));
    }
    Ok(())
}

/// #1559: gate for `profile operation=grant_reality_write`. Requires the exact
/// strictest existing escalation posture — explicit operator confirmation
/// (`confirm_break_glass=true`), a non-empty reason, and that THIS session owns
/// the foreground input lease (matching full_capability/break_glass). Fail-closed:
/// any unmet gate returns an error and the caller never installs the overlay.
fn validate_reality_write_grant_gate(
    session_id: &str,
    reason: Option<&str>,
    confirm_break_glass: bool,
    lease_proof: &ToolProfileLeaseProof,
) -> Result<(), ErrorData> {
    if !confirm_break_glass {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "profile operation=grant_reality_write requires confirm_break_glass=true",
        ));
    }
    if reason.is_none_or(str::is_empty) {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "profile operation=grant_reality_write requires a non-empty reason",
        ));
    }
    if !lease_proof.caller_is_owner {
        return Err(ErrorData::new(
            ErrorCode(-32099),
            format!(
                "profile operation=grant_reality_write requires this MCP session to own the foreground input lease; current owner={:?}",
                lease_proof.owner_session_id
            ),
            Some(json!({
                "code": error_codes::ACTION_FOREGROUND_LEASE_NOT_HELD,
                "session_id": session_id,
                "lease_proof": lease_proof,
                "resolution": "call act operation=lease_acquire first, then retry profile operation=grant_reality_write with confirm_break_glass=true and a non-empty reason",
            })),
        ));
    }
    Ok(())
}

fn break_glass_lease_proof(session_id: &str, profile: ToolProfileKind) -> ToolProfileLeaseProof {
    let status = lease::status();
    ToolProfileLeaseProof {
        required: matches!(
            profile,
            ToolProfileKind::BreakGlass | ToolProfileKind::FullCapability
        ),
        // FullCapability is auto-assigned to the trusted local-model harness and
        // does not gate on the foreground lease; only an *explicit*
        // tool_profile_set escalation (handled in validate_profile_set_policy)
        // is lease-gated. See [`validate_profile_set_policy`].
        held: status.held,
        owner_session_id: status.owner_session_id.clone(),
        caller_is_owner: status.owner_session_id.as_deref() == Some(session_id),
        expires_in_ms: status.expires_in_ms,
    }
}

fn normalize_reason(raw: Option<&str>) -> Result<Option<String>, ErrorData> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let trimmed = raw.trim();
    if trimmed.chars().count() > MAX_PROFILE_REASON_CHARS {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("tool profile reason must be at most {MAX_PROFILE_REASON_CHARS} characters"),
        ));
    }
    Ok((!trimmed.is_empty()).then(|| trimmed.to_owned()))
}

fn profile_facade_error(
    operation: ProfileOperation,
    message: &'static str,
    remediation: &'static str,
) -> ErrorData {
    ErrorData::new(
        ErrorCode(-32099),
        message.to_owned(),
        Some(json!({
            "code": error_codes::TOOL_PARAMS_INVALID,
            "operation": operation.as_str(),
            "source_of_truth": TOOL_PROFILE_SOURCE_OF_TRUTH,
            "remediation": remediation,
        })),
    )
}

fn visible_tool_names_for_profile(
    profile: ToolProfileKind,
    full_tool_names: &[String],
) -> Vec<String> {
    let mut names = full_tool_names
        .iter()
        .filter(|name| profile.is_visible(name))
        .cloned()
        .collect::<Vec<_>>();
    names.sort_by(|left, right| {
        tool_rank(profile, left)
            .cmp(&tool_rank(profile, right))
            .then(left.cmp(right))
    });
    names
}

fn denied_break_glass_tools(visible_tool_names: &[String]) -> Vec<String> {
    let visible = visible_tool_names
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    BREAK_GLASS_HAZARDOUS_TOOLS
        .iter()
        .copied()
        .filter(|name| !visible.contains(name))
        .map(str::to_owned)
        .collect()
}

/// #1352: compute THIS session's current standing on the real OS-foreground
/// route — whether it already holds the lease and a foreground-capable profile,
/// and the precise remaining escalation steps. A read-only preflight so an agent
/// (or the operator) can see the gate state before attempting a foreground action.
fn foreground_route_readiness(
    session_id: Option<&str>,
    profile: ToolProfileKind,
) -> ToolProfileForegroundRoute {
    let holds_foreground_lease = match session_id {
        Some(session_id) => {
            synapse_action::lease::status().owner_session_id.as_deref() == Some(session_id)
        }
        // Unscoped stdio admin runs without a session lease concept.
        None => true,
    };
    let profile_allows_foreground = matches!(
        profile,
        ToolProfileKind::BreakGlass | ToolProfileKind::FullCapability
    );
    let mut remaining_steps = Vec::new();
    if !holds_foreground_lease {
        remaining_steps.push(
            "act operation=lease_acquire (this session must own the foreground input lease)"
                .to_owned(),
        );
    }
    if !profile_allows_foreground {
        remaining_steps.push(
            "profile operation=set profile=break_glass confirm_break_glass=true reason=<why> (requires the lease first)"
                .to_owned(),
        );
    }
    ToolProfileForegroundRoute {
        holds_foreground_lease,
        profile_allows_foreground,
        foreground_route_ready: holds_foreground_lease && profile_allows_foreground,
        remaining_steps,
    }
}

fn foreground_capability_policy(profile: ToolProfileKind) -> ToolProfileForegroundCapability {
    let (preferred_path, real_os_foreground_path) = match profile {
        ToolProfileKind::NormalAgent => (
            "only registered public facade tools are visible in the normal profile; implementation tools require an explicit advanced profile or a facade route",
            "act operation=lease_acquire + profile operation=set break_glass + raw foreground primitive; denied without lease/reason/confirm",
        ),
        ToolProfileKind::BrowserControl => (
            "the <=40 public browser/action facades are visible in the task profile; raw implementation browser tools stay hidden behind those facades",
            "act operation=lease_acquire + profile operation=set break_glass + raw foreground primitive; denied without lease/reason/confirm",
        ),
        ToolProfileKind::BrowserDebugger => (
            "the browser_debugger facade is visible by default and its raw CDP/chrome.debugger operations are enabled only by this explicit profile",
            "act operation=lease_acquire + profile operation=set break_glass + raw foreground primitive; denied without lease/reason/confirm",
        ),
        ToolProfileKind::BreakGlass | ToolProfileKind::FullCapability => (
            "the <=40 public facade surface stays visible; broader authority is routed through facade operations and audited action guards",
            "raw foreground implementation primitives are not discoverable; use act/target facades so lease/target guards and action audit always run",
        ),
    };
    ToolProfileForegroundCapability {
        source_of_truth: TOOL_PROFILE_SOURCE_OF_TRUTH,
        profile_preserves_capability: true,
        human_os_foreground: "the physical foreground window used by the human; never an implicit fallback for hidden tools",
        agent_logical_foreground: "the per-session foreground-equivalent target/lane; preferred for valid local work",
        preferred_path,
        real_os_foreground_path,
    }
}

fn hidden_tool_capability_routes(visible_tool_names: &[String]) -> Vec<HiddenToolCapabilityRoute> {
    let visible = visible_tool_names
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    BREAK_GLASS_HAZARDOUS_TOOLS
        .iter()
        .chain(BROWSER_DEBUGGER_ONLY_EXACT.iter())
        .copied()
        .filter(|name| !visible.contains(name))
        .map(hidden_tool_capability_route)
        .collect()
}

fn hidden_tool_capability_route(tool_name: &str) -> HiddenToolCapabilityRoute {
    let preferred_tools = match tool_name {
        "act_click" => vec![
            "act operation=invoke click",
            "browser_dom operation=locate",
            "target operation=set",
        ],
        "act_type" | "act_set_value" | "act_set_field_text" => {
            vec![
                "act operation=invoke set_field",
                "browser_form operation=set_value",
                "browser_dom operation=locate",
            ]
        }
        "act_press" | "act_keymap" | "act_combo" => {
            vec!["act operation=invoke press", "browser_dom operation=locate"]
        }
        "act_scroll" => vec![
            "act operation=invoke scroll",
            "browser_dom operation=locate",
            "browser_capture operation=screenshot",
            "observe operation=current",
            "target operation=set",
        ],
        "act_stroke" | "act_pad" => vec![
            "target operation=set",
            "act operation=lease_acquire",
            "profile operation=set profile=break_glass",
        ],
        "act_focus_window" => vec![
            "target operation=set",
            "act operation=lease_acquire",
            "profile operation=set profile=break_glass",
            "act operation=invoke focus_window",
            "session operation=list",
        ],
        "act_launch" => vec![
            "agent operation=spawn",
            "browser_tabs operation=new",
            "browser_nav operation=navigate",
        ],
        "act_clipboard" => vec![
            "workspace operation=put",
            "browser_form operation=set_value",
            "act operation=invoke set_field",
        ],
        "release_all" => vec![
            "target operation=set",
            "act operation=lease_release",
            "session operation=list",
        ],
        "hidden_desktop_pip_frame" => {
            vec![
                "screenshot operation=capture",
                "observe operation=current",
                "session operation=list",
            ]
        }
        "action_diagnostic_queue_full_setup" | "action_diagnostic_rate_limit_override" => {
            vec![
                "health",
                "storage operation=inspect",
                "session operation=list",
            ]
        }
        "profile" => vec!["tool_profile_status", "tool_profile_set"],
        "tool_profile_set" | "tool_profile_status" => {
            vec!["profile operation=status", "profile operation=set"]
        }
        "browser_console_messages"
        | "browser_network"
        | "browser_network_har"
        | "browser_network_overrides"
        | "browser_route" => vec![
            "profile operation=set profile=browser_debugger confirm_break_glass=true reason=<why raw CDP is required>",
            "browser_debugger operation=<matching operation>",
            "browser_dom operation=locate",
            "browser_wait operation=for_condition",
            "browser_storage operation=read",
        ],
        tool if BROWSER_DEBUGGER_ONLY_EXACT.contains(&tool) => vec![
            "profile operation=set profile=browser_debugger confirm_break_glass=true reason=<why chrome.debugger is required>",
            "browser_debugger operation=<matching operation>",
            "browser_tabs operation=list",
            "browser_dom operation=locate",
            "act operation=invoke",
        ],
        _ => vec![
            "act operation=invoke",
            "profile operation=set profile=break_glass",
        ],
    };
    HiddenToolCapabilityRoute {
        hidden_tool: tool_name.to_owned(),
        status: "routed_or_break_glass",
        preferred_tools: preferred_tools.into_iter().map(str::to_owned).collect(),
        agent_logical_foreground_policy: "use the preferred tools against this session's agent_logical_foreground/foreground_lane",
        human_os_foreground_policy: "never use the human OS foreground as an implicit fallback",
        break_glass_policy: "for browser CDP/chrome.debugger instrumentation, call profile operation=set profile=browser_debugger with confirm_break_glass=true and a non-empty reason; for a real OS foreground primitive, first acquire the input lease, then call profile operation=set profile=break_glass with confirm_break_glass=true and a non-empty reason",
    }
}

#[derive(Debug, Eq, PartialEq)]
struct PublicToolRegistryValidation {
    duplicate_public_tool_names: Vec<String>,
    forbidden_public_tool_names: Vec<String>,
    over_limit_by: usize,
}

impl PublicToolRegistryValidation {
    const fn is_valid(&self) -> bool {
        self.duplicate_public_tool_names.is_empty()
            && self.forbidden_public_tool_names.is_empty()
            && self.over_limit_by == 0
    }
}

#[derive(Debug, Eq, PartialEq)]
struct FacadeContractValidation {
    missing_contract_tool_names: Vec<String>,
    unknown_contract_tool_names: Vec<String>,
    duplicate_contract_tool_names: Vec<String>,
    duplicate_operation_names: Vec<String>,
    invalid_contract_reasons: Vec<String>,
}

impl FacadeContractValidation {
    const fn is_valid(&self) -> bool {
        self.missing_contract_tool_names.is_empty()
            && self.unknown_contract_tool_names.is_empty()
            && self.duplicate_contract_tool_names.is_empty()
            && self.duplicate_operation_names.is_empty()
            && self.invalid_contract_reasons.is_empty()
    }
}

fn public_tool_names() -> Vec<String> {
    PUBLIC_TOOL_NAMES
        .iter()
        .map(|name| (*name).to_owned())
        .collect()
}

fn public_tool_registry_snapshot_for(
    full_tool_names: &[String],
) -> Result<PublicToolRegistrySnapshot, ErrorData> {
    let public_tool_names = public_tool_names();
    let validation = validate_public_tool_registry_names(&public_tool_names)?;
    let full_tool_names = full_tool_names
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    let registered_tools_present = public_tool_names
        .iter()
        .filter(|name| full_tool_names.contains(name.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    let registered_tools_missing = public_tool_names
        .iter()
        .filter(|name| !full_tool_names.contains(name.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    Ok(PublicToolRegistrySnapshot {
        source_of_truth: PUBLIC_TOOL_REGISTRY_SOURCE_OF_TRUTH,
        max_public_tool_count: PUBLIC_TOOL_LIMIT,
        public_tool_count: public_tool_names.len(),
        public_tool_sha256: sha256_json_hex(&public_tool_names)?,
        public_tool_names,
        implementation_tool_count: full_tool_names.len(),
        registered_tools_present,
        registered_tools_missing,
        duplicate_public_tool_names: validation.duplicate_public_tool_names,
        forbidden_public_tool_names: validation.forbidden_public_tool_names,
        over_limit_by: validation.over_limit_by,
    })
}

fn validate_public_tool_registry_names(
    names: &[String],
) -> Result<PublicToolRegistryValidation, ErrorData> {
    let validation = inspect_public_tool_registry_names(names);
    if validation.is_valid() {
        return Ok(validation);
    }
    Err(public_tool_registry_error(names, &validation))
}

fn inspect_public_tool_registry_names(names: &[String]) -> PublicToolRegistryValidation {
    let mut seen = BTreeSet::new();
    let mut duplicate_public_tool_names = BTreeSet::new();
    for name in names {
        if !seen.insert(name.as_str()) {
            duplicate_public_tool_names.insert(name.clone());
        }
    }
    let forbidden_public_tool_names = names
        .iter()
        .filter(|name| PUBLIC_TOOL_IMPLEMENTATION_DENYLIST.contains(&name.as_str()))
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    PublicToolRegistryValidation {
        duplicate_public_tool_names: duplicate_public_tool_names.into_iter().collect(),
        forbidden_public_tool_names,
        over_limit_by: names.len().saturating_sub(PUBLIC_TOOL_LIMIT),
    }
}

fn public_tool_registry_error(
    names: &[String],
    validation: &PublicToolRegistryValidation,
) -> ErrorData {
    ErrorData::new(
        ErrorCode(-32099),
        format!(
            "public MCP tool registry is invalid: count={} max={} duplicates={:?} forbidden={:?}",
            names.len(),
            PUBLIC_TOOL_LIMIT,
            validation.duplicate_public_tool_names,
            validation.forbidden_public_tool_names
        ),
        Some(json!({
            "code": error_codes::TOOL_INTERNAL_ERROR,
            "detail_code": "PUBLIC_TOOL_REGISTRY_INVALID",
            "operation": PUBLIC_TOOL_REGISTRY_OPERATION,
            "source_of_truth": PUBLIC_TOOL_REGISTRY_SOURCE_OF_TRUTH,
            "max_public_tool_count": PUBLIC_TOOL_LIMIT,
            "public_tool_count": names.len(),
            "over_limit_by": validation.over_limit_by,
            "duplicate_public_tool_names": validation.duplicate_public_tool_names,
            "forbidden_public_tool_names": validation.forbidden_public_tool_names,
            "remediation": "edit PUBLIC_TOOL_NAMES so it has at most 40 unique facade names and no implementation-only tools",
        })),
    )
}

fn facade_contract_snapshot_for(
    public_tool_names: &[String],
) -> Result<FacadeContractSnapshot, ErrorData> {
    let validation = validate_facade_contracts(public_tool_names, FACADE_TOOL_CONTRACTS)?;
    let contracts = facade_contract_snapshots(FACADE_TOOL_CONTRACTS);
    let contract_tool_names = contracts
        .iter()
        .map(|contract| contract.tool_name.to_owned())
        .collect::<Vec<_>>();
    let operation_count = contracts
        .iter()
        .map(|contract| contract.operations.len())
        .sum::<usize>();
    let mutating_operation_count = contracts
        .iter()
        .flat_map(|contract| &contract.operations)
        .filter(|operation| operation.mutates_state)
        .count();
    let facade_contract_sha256 = sha256_json_hex(&contracts)?;
    Ok(FacadeContractSnapshot {
        source_of_truth: FACADE_CONTRACT_SOURCE_OF_TRUTH,
        structured_error_contract: FACADE_CONTRACT_STRUCTURED_ERROR,
        public_tool_count: public_tool_names.len(),
        contract_tool_count: contracts.len(),
        operation_count,
        mutating_operation_count,
        facade_contract_sha256,
        contract_tool_names,
        missing_contract_tool_names: validation.missing_contract_tool_names,
        unknown_contract_tool_names: validation.unknown_contract_tool_names,
        duplicate_contract_tool_names: validation.duplicate_contract_tool_names,
        duplicate_operation_names: validation.duplicate_operation_names,
        invalid_contract_reasons: validation.invalid_contract_reasons,
        contracts,
    })
}

fn facade_contract_snapshots(
    contracts: &[FacadeToolContractSpec],
) -> Vec<FacadeToolContractSnapshot> {
    contracts
        .iter()
        .map(|contract| FacadeToolContractSnapshot {
            tool_name: contract.tool_name,
            operation_enum: contract.operation_enum,
            source_of_truth: contract.source_of_truth,
            operations: contract
                .operations
                .iter()
                .map(|operation| FacadeOperationContractSnapshot {
                    operation: operation.operation,
                    mutates_state: operation.mutates_state,
                    target_required: operation.target_required,
                    source_of_truth: operation.source_of_truth,
                    readback_source_of_truth: operation.readback_source_of_truth,
                    error_code: operation.error_code,
                    remediation: operation.remediation,
                })
                .collect(),
        })
        .collect()
}

fn validate_facade_contracts(
    public_tool_names: &[String],
    contracts: &[FacadeToolContractSpec],
) -> Result<FacadeContractValidation, ErrorData> {
    let validation = inspect_facade_contracts(public_tool_names, contracts);
    if validation.is_valid() {
        return Ok(validation);
    }
    Err(facade_contract_error(
        public_tool_names,
        contracts,
        &validation,
    ))
}

fn inspect_facade_contracts(
    public_tool_names: &[String],
    contracts: &[FacadeToolContractSpec],
) -> FacadeContractValidation {
    let public_name_set = public_tool_names
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    let contract_names = contracts
        .iter()
        .map(|contract| contract.tool_name)
        .collect::<Vec<_>>();
    let contract_name_set = contract_names.iter().copied().collect::<BTreeSet<_>>();
    let missing_contract_tool_names = public_tool_names
        .iter()
        .filter(|name| !contract_name_set.contains(name.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    let unknown_contract_tool_names = contract_name_set
        .iter()
        .filter(|name| !public_name_set.contains(**name))
        .map(|name| (*name).to_owned())
        .collect::<Vec<_>>();

    let mut seen_contract_names = BTreeSet::new();
    let mut duplicate_contract_tool_names = BTreeSet::new();
    let mut duplicate_operation_names = BTreeSet::new();
    let mut invalid_contract_reasons = Vec::new();
    for contract in contracts {
        if contract.tool_name.trim().is_empty() {
            invalid_contract_reasons.push("contract tool_name must not be empty".to_owned());
        }
        if !seen_contract_names.insert(contract.tool_name) {
            duplicate_contract_tool_names.insert(contract.tool_name.to_owned());
        }
        if contract.operation_enum.trim().is_empty() {
            invalid_contract_reasons.push(format!(
                "{} operation_enum must not be empty",
                contract.tool_name
            ));
        }
        if contract.source_of_truth.trim().is_empty() {
            invalid_contract_reasons.push(format!(
                "{} source_of_truth must not be empty",
                contract.tool_name
            ));
        }
        if contract.operations.is_empty() {
            invalid_contract_reasons.push(format!(
                "{} must declare at least one operation",
                contract.tool_name
            ));
        }

        let mut seen_operations = BTreeSet::new();
        for operation in contract.operations {
            if operation.operation.trim().is_empty() {
                invalid_contract_reasons.push(format!(
                    "{} operation name must not be empty",
                    contract.tool_name
                ));
            }
            if !seen_operations.insert(operation.operation) {
                duplicate_operation_names
                    .insert(format!("{}.{}", contract.tool_name, operation.operation));
            }
            if operation.source_of_truth.trim().is_empty() {
                invalid_contract_reasons.push(format!(
                    "{}.{} source_of_truth must not be empty",
                    contract.tool_name, operation.operation
                ));
            }
            if operation.error_code.trim().is_empty() {
                invalid_contract_reasons.push(format!(
                    "{}.{} error_code must not be empty",
                    contract.tool_name, operation.operation
                ));
            }
            if operation.remediation.trim().is_empty() {
                invalid_contract_reasons.push(format!(
                    "{}.{} remediation must not be empty",
                    contract.tool_name, operation.operation
                ));
            }
            if operation.mutates_state
                && operation
                    .readback_source_of_truth
                    .is_none_or(|readback| readback.trim().is_empty())
            {
                invalid_contract_reasons.push(format!(
                    "{}.{} mutates_state requires readback_source_of_truth",
                    contract.tool_name, operation.operation
                ));
            }
        }
    }

    FacadeContractValidation {
        missing_contract_tool_names,
        unknown_contract_tool_names,
        duplicate_contract_tool_names: duplicate_contract_tool_names.into_iter().collect(),
        duplicate_operation_names: duplicate_operation_names.into_iter().collect(),
        invalid_contract_reasons,
    }
}

fn facade_contract_error(
    public_tool_names: &[String],
    contracts: &[FacadeToolContractSpec],
    validation: &FacadeContractValidation,
) -> ErrorData {
    ErrorData::new(
        ErrorCode(-32099),
        format!(
            "public facade contract is invalid: public_count={} contract_count={} missing={:?} unknown={:?} duplicate_tools={:?} duplicate_operations={:?} invalid_reasons={:?}",
            public_tool_names.len(),
            contracts.len(),
            validation.missing_contract_tool_names,
            validation.unknown_contract_tool_names,
            validation.duplicate_contract_tool_names,
            validation.duplicate_operation_names,
            validation.invalid_contract_reasons
        ),
        Some(json!({
            "code": error_codes::TOOL_INTERNAL_ERROR,
            "detail_code": FACADE_CONTRACT_ERROR_CODE,
            "operation": FACADE_CONTRACT_OPERATION,
            "source_of_truth": FACADE_CONTRACT_SOURCE_OF_TRUTH,
            "structured_error_contract": FACADE_CONTRACT_STRUCTURED_ERROR,
            "public_tool_count": public_tool_names.len(),
            "contract_tool_count": contracts.len(),
            "missing_contract_tool_names": validation.missing_contract_tool_names,
            "unknown_contract_tool_names": validation.unknown_contract_tool_names,
            "duplicate_contract_tool_names": validation.duplicate_contract_tool_names,
            "duplicate_operation_names": validation.duplicate_operation_names,
            "invalid_contract_reasons": validation.invalid_contract_reasons,
            "remediation": "edit FACADE_TOOL_CONTRACTS so every public facade has unique typed operations, source_of_truth, structured error code, remediation, and readback_source_of_truth for mutations",
        })),
    )
}

fn sort_tools_for_profile(tools: &mut [Tool], profile: ToolProfileKind) {
    tools.sort_by(|left, right| {
        let left_name = left.name.as_ref();
        let right_name = right.name.as_ref();
        tool_rank(profile, left_name)
            .cmp(&tool_rank(profile, right_name))
            .then(left_name.cmp(right_name))
    });
}

fn tool_rank(profile: ToolProfileKind, tool_name: &str) -> usize {
    match profile {
        ToolProfileKind::NormalAgent => NORMAL_ALLOWED_EXACT
            .iter()
            .position(|name| *name == tool_name)
            .unwrap_or(usize::MAX),
        ToolProfileKind::BrowserControl => BROWSER_CONTROL_ALLOWED_EXACT
            .iter()
            .position(|name| *name == tool_name)
            .unwrap_or(usize::MAX),
        ToolProfileKind::BrowserDebugger => BROWSER_DEBUGGER_ALLOWED_EXACT
            .iter()
            .position(|name| *name == tool_name)
            .unwrap_or(usize::MAX),
        ToolProfileKind::BreakGlass | ToolProfileKind::FullCapability => PUBLIC_TOOL_NAMES
            .iter()
            .position(|name| *name == tool_name)
            .unwrap_or(usize::MAX),
    }
}

fn tool_profile_key(session_id: &str) -> Vec<u8> {
    format!("{TOOL_PROFILE_PREFIX}{session_id}").into_bytes()
}

fn audit_readback(
    readback: super::command_audit::CommandAuditRowReadback,
) -> ToolProfileAuditReadback {
    ToolProfileAuditReadback {
        cf_name: readback.cf_name,
        key_hex: readback.key_hex,
        value_len_bytes: readback.value_len_bytes,
        value_sha256: readback.value_sha256,
    }
}

fn codex_client_surface_snapshot(public_tool_names: &[String]) -> CodexClientSurfaceSnapshot {
    let host_snapshot = match env_path_checked("APPDATA", ["synapse", "codex-tool-surface.json"]) {
        Ok(path) => codex_tool_surface_snapshot_readback(&path),
        Err(error) => CodexToolSurfaceSnapshotReadback {
            path: "%APPDATA%\\synapse\\codex-tool-surface.json".to_owned(),
            exists: false,
            len_bytes: None,
            sha256: None,
            read_error: Some(error),
            tool_count: None,
            tool_surface_sha256: None,
            tool_names: Vec::new(),
        },
    };
    let mut latest_restart_handoff =
        match env_path_checked("LOCALAPPDATA", ["synapse", "codex-restart-handoffs"]) {
            Ok(path) => latest_codex_restart_handoff(&path),
            Err(error) => Some(CodexRestartHandoffReadback {
                path: "%LOCALAPPDATA%\\synapse\\codex-restart-handoffs".to_owned(),
                exists: false,
                len_bytes: None,
                sha256: None,
                read_error: Some(error),
                created_at_utc: None,
                reason_code: None,
                reason: None,
                phase: None,
                required_restart: false,
                no_in_process_hot_refresh: false,
                stale_codex_pid: None,
                stale_codex_command_line: None,
                active_issue_ref: None,
                daemon_pid: None,
                daemon_bind: None,
                daemon_pid_role: None,
                daemon_pid_authoritative_for_configured_bind: None,
                daemon_tool_count: None,
                daemon_tool_surface_sha256: None,
                current_process_start_snapshot_status: None,
                current_process_start_env_hash: None,
                live_daemon_pid: std::process::id(),
                daemon_pid_matches_live_daemon: None,
                daemon_pid_mismatch_detail: None,
            }),
        };
    let public_tools_missing_from_host_snapshot =
        sorted_missing_names(public_tool_names, &host_snapshot.tool_names);
    let host_snapshot_tools_missing_from_public_registry =
        sorted_missing_names(&host_snapshot.tool_names, public_tool_names);
    let host_snapshot_is_current = host_snapshot.read_error.is_none()
        && host_snapshot.exists
        && public_tools_missing_from_host_snapshot.is_empty();
    let restart_handoff_requires_current = latest_restart_handoff.as_ref().is_some_and(|handoff| {
        host_snapshot_is_current
            && restart_handoff_requires_current_codex_restart(handoff, &host_snapshot)
    });
    let live_stale_codex_process = latest_restart_handoff
        .as_ref()
        .filter(|_| restart_handoff_requires_current)
        .and_then(|handoff| handoff.stale_codex_pid)
        .and_then(live_codex_process_readback);
    if let Some(handoff) = latest_restart_handoff.as_mut() {
        resolve_restart_handoff_current_action_readback(
            handoff,
            host_snapshot_is_current,
            restart_handoff_requires_current,
            live_stale_codex_process.is_some(),
        );
    }

    let (status, diagnostic_code) = if host_snapshot.read_error.is_some() && host_snapshot.exists {
        (
            CodexClientSurfaceStatus::HostSnapshotReadError,
            "CODEX_CLIENT_SURFACE_HOST_SNAPSHOT_READ_ERROR",
        )
    } else if !host_snapshot.exists {
        (
            CodexClientSurfaceStatus::HostSnapshotMissing,
            "CODEX_CLIENT_SURFACE_HOST_SNAPSHOT_MISSING",
        )
    } else if !public_tools_missing_from_host_snapshot.is_empty() {
        (
            CodexClientSurfaceStatus::HostSnapshotMissingPublicTools,
            "CODEX_CLIENT_SURFACE_HOST_SNAPSHOT_TOOL_MISMATCH",
        )
    } else if latest_restart_handoff
        .as_ref()
        .is_some_and(|handoff| handoff.read_error.is_some())
    {
        (
            CodexClientSurfaceStatus::HandoffReadError,
            "SYNAPSE_CODEX_RESTART_HANDOFF_READ_ERROR",
        )
    } else if live_stale_codex_process.is_some() {
        (
            CodexClientSurfaceStatus::RestartRequiredForLiveCodexPid,
            "SYNAPSE_CODEX_CURRENT_PROCESS_SCHEMA_STALE",
        )
    } else if latest_restart_handoff.as_ref().is_some_and(|handoff| {
        restart_handoff_requires_current_codex_restart(handoff, &host_snapshot)
    }) {
        (
            CodexClientSurfaceStatus::RestartHandoffPresentForDeadPid,
            "SYNAPSE_CODEX_RESTART_HANDOFF_STALE_PID_DEAD",
        )
    } else {
        (
            CodexClientSurfaceStatus::HostSnapshotMatchesPublicTools,
            "CODEX_CLIENT_SURFACE_OK",
        )
    };

    CodexClientSurfaceSnapshot {
        source_of_truth: CODEX_CLIENT_SURFACE_SOURCE_OF_TRUTH,
        status,
        diagnostic_code,
        remediation: CODEX_CLIENT_SURFACE_REMEDIATION,
        host_snapshot,
        latest_restart_handoff,
        live_stale_codex_process,
        public_tools_missing_from_host_snapshot,
        host_snapshot_tools_missing_from_public_registry,
    }
}

fn restart_handoff_requires_current_codex_restart(
    handoff: &CodexRestartHandoffReadback,
    host_snapshot: &CodexToolSurfaceSnapshotReadback,
) -> bool {
    if !handoff.required_restart {
        return false;
    }

    !restart_handoff_start_hash_matches_host_snapshot(handoff, host_snapshot)
}

fn resolve_restart_handoff_current_action_readback(
    handoff: &mut CodexRestartHandoffReadback,
    host_snapshot_is_current: bool,
    restart_handoff_requires_current: bool,
    live_stale_codex_process_present: bool,
) {
    if handoff.read_error.is_some() || !host_snapshot_is_current || !handoff.required_restart {
        return;
    }
    if restart_handoff_requires_current && live_stale_codex_process_present {
        return;
    }
    // The persisted handoff file remains the raw audit artifact via path/sha.
    // The profile/telemetry payload reports whether action is required now.
    handoff.required_restart = false;
    handoff.no_in_process_hot_refresh = false;
}

fn restart_handoff_start_hash_matches_host_snapshot(
    handoff: &CodexRestartHandoffReadback,
    host_snapshot: &CodexToolSurfaceSnapshotReadback,
) -> bool {
    let Some(start_hash) = handoff.current_process_start_env_hash.as_deref() else {
        return false;
    };
    let Some(host_hash) = host_snapshot.tool_surface_sha256.as_deref() else {
        return false;
    };
    tool_surface_hashes_match(start_hash, host_hash)
}

fn tool_surface_hashes_match(left: &str, right: &str) -> bool {
    fn canonical(hash: &str) -> &str {
        hash.trim().strip_prefix("sha256:").unwrap_or(hash.trim())
    }

    let left = canonical(left);
    let right = canonical(right);
    !left.is_empty() && !right.is_empty() && left.eq_ignore_ascii_case(right)
}

fn codex_tool_surface_snapshot_readback(path: &Path) -> CodexToolSurfaceSnapshotReadback {
    let path_text = path.display().to_string();
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return CodexToolSurfaceSnapshotReadback {
                path: path_text,
                exists: false,
                len_bytes: None,
                sha256: None,
                read_error: None,
                tool_count: None,
                tool_surface_sha256: None,
                tool_names: Vec::new(),
            };
        }
        Err(error) => {
            return CodexToolSurfaceSnapshotReadback {
                path: path_text,
                exists: false,
                len_bytes: None,
                sha256: None,
                read_error: Some(format!("read failed: {error}")),
                tool_count: None,
                tool_surface_sha256: None,
                tool_names: Vec::new(),
            };
        }
    };
    let sha256 = sha256_hex(&bytes);
    let len_bytes = bytes.len() as u64;
    let value = match serde_json::from_slice::<Value>(&bytes) {
        Ok(value) => value,
        Err(error) => {
            return CodexToolSurfaceSnapshotReadback {
                path: path_text,
                exists: true,
                len_bytes: Some(len_bytes),
                sha256: Some(sha256),
                read_error: Some(format!("json parse failed: {error}")),
                tool_count: None,
                tool_surface_sha256: None,
                tool_names: Vec::new(),
            };
        }
    };
    CodexToolSurfaceSnapshotReadback {
        path: path_text,
        exists: true,
        len_bytes: Some(len_bytes),
        sha256: Some(sha256),
        read_error: None,
        tool_count: json_pointer_usize(&value, "/tool_count"),
        tool_surface_sha256: json_pointer_string(&value, "/tool_surface_sha256"),
        tool_names: json_pointer_string_array(&value, "/tool_names"),
    }
}

fn latest_codex_restart_handoff(dir: &Path) -> Option<CodexRestartHandoffReadback> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
        Err(error) => {
            return Some(CodexRestartHandoffReadback {
                path: dir.display().to_string(),
                exists: false,
                len_bytes: None,
                sha256: None,
                read_error: Some(format!("read_dir failed: {error}")),
                created_at_utc: None,
                reason_code: None,
                reason: None,
                phase: None,
                required_restart: false,
                no_in_process_hot_refresh: false,
                stale_codex_pid: None,
                stale_codex_command_line: None,
                active_issue_ref: None,
                daemon_pid: None,
                daemon_bind: None,
                daemon_pid_role: None,
                daemon_pid_authoritative_for_configured_bind: None,
                daemon_tool_count: None,
                daemon_tool_surface_sha256: None,
                current_process_start_snapshot_status: None,
                current_process_start_env_hash: None,
                live_daemon_pid: std::process::id(),
                daemon_pid_matches_live_daemon: None,
                daemon_pid_mismatch_detail: None,
            });
        }
    };

    let mut newest: Option<(SystemTime, PathBuf)> = None;
    for entry in entries {
        let Ok(entry) = entry else {
            continue;
        };
        let path = entry.path();
        let is_json = path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| extension.eq_ignore_ascii_case("json"));
        let is_handoff = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("codex-restart-handoff-"));
        if !is_json || !is_handoff {
            continue;
        }
        let modified = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        if newest
            .as_ref()
            .is_none_or(|(current_modified, _)| modified > *current_modified)
        {
            newest = Some((modified, path));
        }
    }
    let (_, path) = newest?;
    Some(codex_restart_handoff_readback(&path))
}

fn codex_restart_handoff_readback(path: &Path) -> CodexRestartHandoffReadback {
    let path_text = path.display().to_string();
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) => {
            return CodexRestartHandoffReadback {
                path: path_text,
                exists: false,
                len_bytes: None,
                sha256: None,
                read_error: Some(format!("read failed: {error}")),
                created_at_utc: None,
                reason_code: None,
                reason: None,
                phase: None,
                required_restart: false,
                no_in_process_hot_refresh: false,
                stale_codex_pid: None,
                stale_codex_command_line: None,
                active_issue_ref: None,
                daemon_pid: None,
                daemon_bind: None,
                daemon_pid_role: None,
                daemon_pid_authoritative_for_configured_bind: None,
                daemon_tool_count: None,
                daemon_tool_surface_sha256: None,
                current_process_start_snapshot_status: None,
                current_process_start_env_hash: None,
                live_daemon_pid: std::process::id(),
                daemon_pid_matches_live_daemon: None,
                daemon_pid_mismatch_detail: None,
            };
        }
    };
    let sha256 = sha256_hex(&bytes);
    let len_bytes = bytes.len() as u64;
    let value = match serde_json::from_slice::<Value>(&bytes) {
        Ok(value) => value,
        Err(error) => {
            return CodexRestartHandoffReadback {
                path: path_text,
                exists: true,
                len_bytes: Some(len_bytes),
                sha256: Some(sha256),
                read_error: Some(format!("json parse failed: {error}")),
                created_at_utc: None,
                reason_code: None,
                reason: None,
                phase: None,
                required_restart: false,
                no_in_process_hot_refresh: false,
                stale_codex_pid: None,
                stale_codex_command_line: None,
                active_issue_ref: None,
                daemon_pid: None,
                daemon_bind: None,
                daemon_pid_role: None,
                daemon_pid_authoritative_for_configured_bind: None,
                daemon_tool_count: None,
                daemon_tool_surface_sha256: None,
                current_process_start_snapshot_status: None,
                current_process_start_env_hash: None,
                live_daemon_pid: std::process::id(),
                daemon_pid_matches_live_daemon: None,
                daemon_pid_mismatch_detail: None,
            };
        }
    };
    let live_daemon_pid = std::process::id();
    let daemon_pid = json_pointer_u32(&value, "/daemon/pid");
    let daemon_pid_matches_live_daemon = daemon_pid.map(|pid| pid == live_daemon_pid);
    let daemon_pid_role = json_pointer_string(&value, "/daemon/pid_role");
    let daemon_pid_mismatch_detail =
        daemon_pid_matches_live_daemon.and_then(|matches_live_daemon| {
            if matches_live_daemon {
                None
            } else {
                Some(format!(
                    "handoff daemon pid {} does not match live daemon pid {} serving this profile/telemetry request; phase={} role={}",
                    daemon_pid.unwrap_or_default(),
                    live_daemon_pid,
                    json_pointer_string(&value, "/phase").unwrap_or_else(|| "unknown".to_owned()),
                    daemon_pid_role.as_deref().unwrap_or("unspecified")
                ))
            }
        });
    CodexRestartHandoffReadback {
        path: path_text,
        exists: true,
        len_bytes: Some(len_bytes),
        sha256: Some(sha256),
        read_error: None,
        created_at_utc: json_pointer_string(&value, "/created_at_utc"),
        reason_code: json_pointer_string(&value, "/reason_code"),
        reason: json_pointer_string(&value, "/reason"),
        phase: json_pointer_string(&value, "/phase"),
        required_restart: json_pointer_bool(&value, "/required_restart").unwrap_or(false),
        no_in_process_hot_refresh: json_pointer_bool(&value, "/no_in_process_hot_refresh")
            .unwrap_or(false),
        stale_codex_pid: json_pointer_u32(&value, "/codex_process/pid"),
        stale_codex_command_line: json_pointer_string(&value, "/codex_process/command_line"),
        active_issue_ref: json_pointer_string(&value, "/active_issue/issue_ref"),
        daemon_pid,
        daemon_bind: json_pointer_string(&value, "/daemon/bind"),
        daemon_pid_role,
        daemon_pid_authoritative_for_configured_bind: json_pointer_bool(
            &value,
            "/daemon/pid_authoritative_for_configured_bind",
        ),
        daemon_tool_count: json_pointer_usize(&value, "/daemon/tool_count"),
        daemon_tool_surface_sha256: json_pointer_string(&value, "/daemon/tool_surface_sha256"),
        current_process_start_snapshot_status: json_pointer_string(
            &value,
            "/current_process_start_surface/snapshot_status",
        ),
        current_process_start_env_hash: json_pointer_string(
            &value,
            "/current_process_start_surface/env_hash",
        ),
        live_daemon_pid,
        daemon_pid_matches_live_daemon,
        daemon_pid_mismatch_detail,
    }
}

fn live_codex_process_readback(pid: u32) -> Option<CodexProcessReadback> {
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

    let sys_pid = sysinfo::Pid::from_u32(pid);
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[sys_pid]),
        true,
        ProcessRefreshKind::nothing()
            .with_cmd(UpdateKind::Always)
            .with_exe(UpdateKind::Always),
    );
    let process = system.process(sys_pid)?;
    let name = process.name().to_string_lossy().into_owned();
    if !name.eq_ignore_ascii_case("codex.exe") && !name.eq_ignore_ascii_case("codex") {
        return None;
    }
    Some(CodexProcessReadback {
        source_of_truth: "live OS process table via sysinfo refresh_processes_specifics",
        pid,
        parent_pid: process.parent().map(|parent| parent.as_u32()),
        name,
        exe: process.exe().map(|path| path.display().to_string()),
        command_line: process
            .cmd()
            .iter()
            .map(|part| part.to_string_lossy())
            .collect::<Vec<_>>()
            .join(" "),
    })
}

fn sorted_missing_names(expected: &[String], actual: &[String]) -> Vec<String> {
    let actual = actual.iter().map(String::as_str).collect::<BTreeSet<_>>();
    expected
        .iter()
        .filter(|name| !actual.contains(name.as_str()))
        .cloned()
        .collect()
}

fn env_path_checked<const N: usize>(env_name: &str, parts: [&str; N]) -> Result<PathBuf, String> {
    let Some(root) = std::env::var_os(env_name) else {
        return Err(format!("{env_name} environment variable is not set"));
    };
    let mut path = PathBuf::from(root);
    for part in parts {
        path.push(part);
    }
    Ok(path)
}

fn json_pointer_string(value: &Value, pointer: &str) -> Option<String> {
    value
        .pointer(pointer)
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

fn json_pointer_bool(value: &Value, pointer: &str) -> Option<bool> {
    value.pointer(pointer).and_then(Value::as_bool)
}

fn json_pointer_usize(value: &Value, pointer: &str) -> Option<usize> {
    value
        .pointer(pointer)
        .and_then(Value::as_u64)
        .and_then(|raw| usize::try_from(raw).ok())
}

fn json_pointer_u32(value: &Value, pointer: &str) -> Option<u32> {
    value
        .pointer(pointer)
        .and_then(Value::as_u64)
        .and_then(|raw| u32::try_from(raw).ok())
}

fn json_pointer_string_array(value: &Value, pointer: &str) -> Vec<String> {
    value
        .pointer(pointer)
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(ToOwned::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

fn sha256_json_hex<T: Serialize>(value: &T) -> Result<String, ErrorData> {
    let bytes = serde_json::to_vec(value).map_err(|error| {
        mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!("serialize tool profile digest payload failed: {error}"),
        )
    })?;
    Ok(sha256_hex(&bytes))
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("sha256:{}", hex_lower(&hasher.finalize()))
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

fn unix_ms_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{num::NonZeroUsize, path::Path};

    use tempfile::TempDir;
    use tokio_util::sync::CancellationToken;

    use crate::{m2::M2ServiceConfig, m3::M3ServiceConfig, m4::M4ServiceConfig};

    fn service_with_db(path: &Path) -> SynapseService {
        SynapseService::try_with_m2_shutdown_reason_and_m3_config(
            CancellationToken::new(),
            "test",
            CancellationToken::new(),
            &M2ServiceConfig::default(),
            M3ServiceConfig::from_cli_parts(
                Some(path.join("db")),
                Some(path.to_path_buf()),
                false,
                "127.0.0.1:0".to_owned(),
                NonZeroUsize::new(4).expect("nonzero"),
                false,
                true,
                None,
                false,
                None,
            ),
            M4ServiceConfig::default(),
        )
        .expect("construct service")
    }

    fn tool_names(tools: Vec<Tool>) -> Vec<String> {
        tools
            .into_iter()
            .map(|tool| tool.name.to_string())
            .collect()
    }

    fn names() -> Vec<String> {
        let mut names = std::collections::BTreeSet::new();
        names.extend(
            BREAK_GLASS_HAZARDOUS_TOOLS
                .iter()
                .map(|name| (*name).to_owned()),
        );
        names.extend(
            BROWSER_DEBUGGER_ONLY_EXACT
                .iter()
                .map(|name| (*name).to_owned()),
        );
        names.extend(
            [
                "act_run_shell",
                "act_run_shell_cancel",
                "act_run_shell_start",
                "act_run_shell_status",
                "act_spawn_agent",
                "agent",
                "agent_ask_operator",
                "agent_inbox",
                "agent_query",
                "agent_send",
                "agent_stats",
                "agent_template_get",
                "agent_wait",
                "approval",
                "approval_decide",
                "approval_gate",
                "approval_list",
                "approval_request",
                "act_foreground",
                "act_launch",
                "act",
                "shell",
                "process",
                "cdp_open_tab",
                "profile",
                "health",
                "session",
                "session_list",
                "screenshot",
                "subscribe",
                "observe",
                "find",
                "read_text",
                "target",
                "target_act",
                "browser_adopt_active_tab",
                "browser_aria_snapshot",
                "browser_assert",
                "browser_batch",
                "browser_capture",
                "browser_clock",
                "browser_content",
                "browser_dom",
                "browser_downloads",
                "browser_file_upload",
                "browser_form",
                "browser_fill_form",
                "browser_frames",
                "browser_inspect",
                "browser_locate",
                "browser_page_events",
                "browser_scroll_into_view",
                "browser_screenshot",
                "browser_set_content",
                "browser_set_value",
                "browser_storage",
                "browser_nav",
                "browser_tabs",
                "browser_wait",
                "browser_wait_for",
                "browser_debugger",
                "control_lease_acquire",
                "control_lease_release",
                "escalation",
                "escalation_ack",
                "escalation_config_get",
                "escalation_config_set",
                "escalation_list",
                "routine",
                "routine_mine",
                "routine_list",
                "routine_inspect",
                "routine_update",
                "routine_feedback",
                "routine_label_export",
                "routine_automate",
                "armed_routine_tick",
                "assist",
                "intent_current",
                "intent_detect_tick",
                "suggestion_tick",
                "suggestion_list",
                "suggestion_accept",
                "reality",
                "reality_baseline",
                "observe_delta",
                "reality_audit",
                "verification",
                "verification_inbox",
                "verification_poll",
                "verification_audit",
                "verification_bind",
                "verification_sources",
                "timeline",
                "timeline_get",
                "timeline_search",
                "timeline_stats",
                "timeline_pause",
                "timeline_resume",
                "timeline_exclusions",
                "timeline_purge",
                "timeline_redact",
                "timeline_digest",
                "episode",
                "episode_list",
                "episode_get",
                "episode_segment",
                "tool_profile_set",
                "tool_profile_status",
                "task",
                "task_create",
                "task_get",
                "task_list",
                "task_update",
                "workspace",
                "workspace_get",
                "workspace_list",
                "workspace_put",
                "workspace_subscribe",
                "privacy",
                "cost",
                "agent_cost",
                "agent_cost_price_put",
                "agent_cost_price_list",
                "agent_cost_price_delete",
                "storage",
                "storage_inspect",
                "storage_put_probe_rows",
                "storage_gc_once",
                "storage_pressure_sample",
                "model",
                "local_model_register",
                "local_model_list",
                "local_model_update",
                "local_model_remove",
                "local_model_probe",
                "hygiene",
                "hygiene_scan_text",
                "hygiene_scan_storage",
                "hygiene_flags",
                "hygiene_report",
                "audit",
                "replay",
                "setup",
                "telemetry",
            ]
            .iter()
            .map(|name| (*name).to_owned()),
        );
        names.into_iter().collect()
    }

    fn assert_debugger_only_hidden(visible: &[String]) {
        for hidden in BROWSER_DEBUGGER_ONLY_EXACT {
            assert!(
                !visible.iter().any(|name| name == hidden),
                "default profile must hide browser debugger tool {hidden}"
            );
        }
    }

    fn assert_public_facade_surface(visible: &[String]) {
        assert_eq!(visible, &public_tool_names());
        assert!(visible.len() <= PUBLIC_TOOL_LIMIT);
        assert!(
            visible
                .iter()
                .all(|name| PUBLIC_TOOL_NAMES.contains(&name.as_str()))
        );
        assert!(visible.contains(&"act".to_owned()));
        assert!(visible.contains(&"profile".to_owned()));
        assert!(visible.contains(&"target".to_owned()));
        assert!(visible.contains(&"browser_dom".to_owned()));
        assert!(visible.contains(&"browser_form".to_owned()));
        assert!(visible.contains(&"browser_capture".to_owned()));
        assert!(visible.contains(&"browser_debugger".to_owned()));
        assert!(!visible.contains(&"target_act".to_owned()));
        assert!(!visible.contains(&"browser_content".to_owned()));
        assert!(!visible.contains(&"browser_file_upload".to_owned()));
        assert!(!visible.contains(&"browser_screenshot".to_owned()));
        assert!(!visible.contains(&"browser_set_value".to_owned()));
        assert!(!visible.contains(&"browser_wait_for".to_owned()));
        assert!(!visible.contains(&"cdp_target_info".to_owned()));
        assert!(!visible.contains(&"tool_profile_set".to_owned()));
        assert!(!visible.contains(&"tool_profile_status".to_owned()));
        assert!(!visible.contains(&"act_click".to_owned()));
        assert!(!visible.contains(&"act_type".to_owned()));
        assert!(!visible.contains(&"release_all".to_owned()));
    }

    fn registry_error_data(error: &ErrorData) -> &Value {
        error.data.as_ref().expect("registry error data")
    }

    const STRICT_CLIENT_SCHEMA_MAP_KEYWORDS: &[&str] = &[
        "$defs",
        "definitions",
        "dependentSchemas",
        "patternProperties",
        "properties",
    ];
    const STRICT_CLIENT_SCHEMA_ARRAY_KEYWORDS: &[&str] =
        &["oneOf", "anyOf", "allOf", "prefixItems"];
    const STRICT_CLIENT_SCHEMA_VALUE_KEYWORDS: &[&str] = &[
        "contains",
        "else",
        "if",
        "items",
        "not",
        "propertyNames",
        "then",
    ];
    const STRICT_CLIENT_BOOLEAN_EXEMPT_KEYS: &[&str] = &[
        "additionalItems",
        "additionalProperties",
        "unevaluatedItems",
        "unevaluatedProperties",
    ];

    fn strict_client_tool_schema_errors(tool: &Tool) -> Vec<String> {
        let mut errors = Vec::new();
        let input = Value::Object((*tool.input_schema).clone());
        if input.get("type") != Some(&json!("object")) {
            errors.push(format!("{}.inputSchema.type is not object", tool.name));
        }
        if input.get("additionalProperties") != Some(&Value::Bool(false)) {
            errors.push(format!(
                "{}.inputSchema.additionalProperties is not false",
                tool.name
            ));
        }
        strict_client_schema_errors(&input, &format!("{}.inputSchema", tool.name), &mut errors);
        if let Some(output) = &tool.output_schema {
            let output = Value::Object((**output).clone());
            strict_client_schema_errors(
                &output,
                &format!("{}.outputSchema", tool.name),
                &mut errors,
            );
        }
        errors
    }

    fn strict_client_schema_errors(value: &Value, path: &str, errors: &mut Vec<String>) {
        match value {
            Value::Bool(_) => errors.push(format!("{path} is a bare boolean schema")),
            Value::Object(map) => {
                if let Some(enum_value) = map.get("enum")
                    && !enum_value
                        .as_array()
                        .is_some_and(|variants| !variants.is_empty())
                {
                    errors.push(format!("{path}.enum is not a non-empty array"));
                }
                for (key, child) in map {
                    let child_path = format!("{path}.{key}");
                    if STRICT_CLIENT_SCHEMA_MAP_KEYWORDS.contains(&key.as_str()) {
                        if let Value::Object(members) = child {
                            for (member_key, member_value) in members {
                                strict_client_subschema_errors(
                                    member_value,
                                    &format!("{child_path}.{member_key}"),
                                    errors,
                                );
                            }
                        } else {
                            strict_client_schema_errors(child, &child_path, errors);
                        }
                    } else if STRICT_CLIENT_SCHEMA_ARRAY_KEYWORDS.contains(&key.as_str()) {
                        if let Value::Array(elements) = child {
                            for (index, element) in elements.iter().enumerate() {
                                strict_client_subschema_errors(
                                    element,
                                    &format!("{child_path}[{index}]"),
                                    errors,
                                );
                            }
                        } else {
                            strict_client_schema_errors(child, &child_path, errors);
                        }
                    } else if STRICT_CLIENT_SCHEMA_VALUE_KEYWORDS.contains(&key.as_str()) {
                        strict_client_subschema_errors(child, &child_path, errors);
                    } else if STRICT_CLIENT_BOOLEAN_EXEMPT_KEYS.contains(&key.as_str()) {
                        if !child.is_boolean() {
                            strict_client_subschema_errors(child, &child_path, errors);
                        }
                    }
                }
            }
            Value::Array(items) => {
                for (index, item) in items.iter().enumerate() {
                    strict_client_schema_errors(item, &format!("{path}[{index}]"), errors);
                }
            }
            _ => {}
        }
    }

    fn strict_client_subschema_errors(value: &Value, path: &str, errors: &mut Vec<String>) {
        if value.is_boolean() {
            errors.push(format!("{path} is a bare boolean schema"));
        } else {
            strict_client_schema_errors(value, path, errors);
        }
    }

    #[test]
    fn public_tool_registry_contract_is_capped_unique_and_facade_only() {
        let public_names = public_tool_names();
        assert_eq!(public_names.len(), PUBLIC_TOOL_LIMIT);
        let validation =
            validate_public_tool_registry_names(&public_names).expect("valid registry");
        assert!(validation.is_valid());
        assert!(public_names.contains(&"health".to_owned()));
        assert!(public_names.contains(&"telemetry".to_owned()));
        assert!(public_names.contains(&"agent".to_owned()));
        assert!(public_names.contains(&"task".to_owned()));
        assert!(public_names.contains(&"approval".to_owned()));
        assert!(public_names.contains(&"escalation".to_owned()));
        assert!(!public_names.contains(&"cdp_open_tab".to_owned()));
        assert!(!public_names.contains(&"agent_query".to_owned()));
        assert!(!public_names.contains(&"agent_stats".to_owned()));
        assert!(!public_names.contains(&"task_create".to_owned()));
        assert!(!public_names.contains(&"task_get".to_owned()));
        assert!(!public_names.contains(&"approval_list".to_owned()));
        assert!(!public_names.contains(&"approval_gate".to_owned()));
        assert!(!public_names.contains(&"escalation_list".to_owned()));
        assert!(!public_names.contains(&"escalation_config_set".to_owned()));
        assert!(!public_names.contains(&"timeline_get".to_owned()));
        assert!(!public_names.contains(&"timeline_search".to_owned()));
        assert!(!public_names.contains(&"timeline_stats".to_owned()));
        assert!(!public_names.contains(&"timeline_purge".to_owned()));
        assert!(!public_names.contains(&"timeline_redact".to_owned()));
        assert!(!public_names.contains(&"episode_list".to_owned()));
        assert!(!public_names.contains(&"episode_get".to_owned()));
        assert!(!public_names.contains(&"routine_mine".to_owned()));
        assert!(!public_names.contains(&"routine_list".to_owned()));
        assert!(!public_names.contains(&"routine_inspect".to_owned()));
        assert!(!public_names.contains(&"routine_update".to_owned()));
        assert!(!public_names.contains(&"routine_feedback".to_owned()));
        assert!(!public_names.contains(&"routine_label_export".to_owned()));
        assert!(!public_names.contains(&"routine_automate".to_owned()));
        assert!(!public_names.contains(&"armed_routine_tick".to_owned()));
        assert!(!public_names.contains(&"intent_current".to_owned()));
        assert!(!public_names.contains(&"intent_detect_tick".to_owned()));
        assert!(!public_names.contains(&"suggestion_tick".to_owned()));
        assert!(!public_names.contains(&"suggestion_list".to_owned()));
        assert!(!public_names.contains(&"suggestion_accept".to_owned()));
        assert!(!public_names.contains(&"reality_baseline".to_owned()));
        assert!(!public_names.contains(&"observe_delta".to_owned()));
        assert!(!public_names.contains(&"reality_audit".to_owned()));
        assert!(!public_names.contains(&"verification_inbox".to_owned()));
        assert!(!public_names.contains(&"verification_poll".to_owned()));
        assert!(!public_names.contains(&"verification_audit".to_owned()));
        assert!(!public_names.contains(&"verification_bind".to_owned()));
        assert!(!public_names.contains(&"verification_sources".to_owned()));
        assert!(!public_names.contains(&"workspace_put".to_owned()));
        assert!(!public_names.contains(&"workspace_get".to_owned()));
        assert!(!public_names.contains(&"storage_put_probe_rows".to_owned()));
        assert!(!public_names.contains(&"storage_inspect".to_owned()));
        assert!(!public_names.contains(&"local_model_list".to_owned()));
        assert!(!public_names.contains(&"local_model_probe".to_owned()));
        assert!(!public_names.contains(&"hygiene_report".to_owned()));
        assert!(!public_names.contains(&"agent_cost".to_owned()));
        assert!(!public_names.contains(&"agent_cost_price_put".to_owned()));
        assert!(!public_names.contains(&"agent_cost_price_list".to_owned()));
        assert!(!public_names.contains(&"agent_cost_price_delete".to_owned()));

        let snapshot =
            public_tool_registry_snapshot_for(&names()).expect("registry snapshot from fixture");
        assert_eq!(snapshot.max_public_tool_count, PUBLIC_TOOL_LIMIT);
        assert_eq!(snapshot.public_tool_count, PUBLIC_TOOL_LIMIT);
        assert!(
            snapshot
                .registered_tools_present
                .contains(&"health".to_owned())
        );
        assert!(
            snapshot
                .registered_tools_present
                .contains(&"profile".to_owned()),
            "#1377 registers the profile facade"
        );
        assert!(
            snapshot
                .registered_tools_present
                .contains(&"session".to_owned()),
            "#1377 registers the session facade"
        );
        assert!(
            snapshot
                .registered_tools_present
                .contains(&"screenshot".to_owned()),
            "#1378 registers the screenshot facade"
        );
        assert!(
            snapshot
                .registered_tools_present
                .contains(&"target".to_owned()),
            "#1379 registers the target facade"
        );
        assert!(
            snapshot
                .registered_tools_present
                .contains(&"act".to_owned()),
            "#1379 registers the act facade"
        );
        assert!(
            snapshot
                .registered_tools_present
                .contains(&"shell".to_owned()),
            "#1380 registers the shell facade"
        );
        assert!(
            snapshot
                .registered_tools_present
                .contains(&"process".to_owned()),
            "#1380 registers the process facade"
        );
        assert!(
            snapshot
                .registered_tools_present
                .contains(&"browser_nav".to_owned()),
            "#1381 registers the browser_nav facade"
        );
        assert!(
            snapshot
                .registered_tools_present
                .contains(&"browser_capture".to_owned()),
            "#1383 registers the browser_capture facade"
        );
        assert!(
            snapshot
                .registered_tools_present
                .contains(&"browser_debugger".to_owned()),
            "#1383 registers the browser_debugger facade"
        );
        assert!(
            snapshot
                .registered_tools_present
                .contains(&"workspace".to_owned()),
            "#1384 registers the workspace facade"
        );
        assert!(
            snapshot
                .registered_tools_present
                .contains(&"agent".to_owned()),
            "#1385 registers the agent facade"
        );
        assert!(
            snapshot
                .registered_tools_present
                .contains(&"task".to_owned()),
            "#1385 registers the task facade"
        );
        assert!(
            snapshot
                .registered_tools_present
                .contains(&"approval".to_owned()),
            "#1386 registers the approval facade"
        );
        assert!(
            snapshot
                .registered_tools_present
                .contains(&"escalation".to_owned()),
            "#1386 registers the escalation facade"
        );
        assert!(
            snapshot
                .registered_tools_present
                .contains(&"timeline".to_owned()),
            "#1387 registers the timeline facade"
        );
        assert!(
            snapshot
                .registered_tools_present
                .contains(&"episode".to_owned()),
            "#1387 registers the episode facade"
        );
        assert!(
            snapshot
                .registered_tools_present
                .contains(&"privacy".to_owned()),
            "#1387 registers the privacy facade"
        );
        assert!(
            snapshot
                .registered_tools_present
                .contains(&"routine".to_owned()),
            "#1388 registers the routine facade"
        );
        assert!(
            snapshot
                .registered_tools_present
                .contains(&"assist".to_owned()),
            "#1388 registers the assist facade"
        );
        assert!(
            snapshot
                .registered_tools_present
                .contains(&"reality".to_owned()),
            "#1388 registers the reality facade"
        );
        assert!(
            snapshot
                .registered_tools_present
                .contains(&"verification".to_owned()),
            "#1388 registers the verification facade"
        );
        assert!(
            snapshot
                .registered_tools_present
                .contains(&"storage".to_owned()),
            "#1389 registers the storage facade"
        );
        assert!(
            snapshot
                .registered_tools_present
                .contains(&"model".to_owned()),
            "#1389 registers the model facade"
        );
        assert!(
            snapshot
                .registered_tools_present
                .contains(&"cost".to_owned()),
            "#1390 registers the cost facade"
        );
        assert!(
            snapshot
                .registered_tools_present
                .contains(&"hygiene".to_owned()),
            "#1389 registers the hygiene facade"
        );
        assert!(
            snapshot
                .registered_tools_present
                .contains(&"setup".to_owned()),
            "#1389 registers the setup facade"
        );
        assert!(
            snapshot
                .registered_tools_present
                .contains(&"telemetry".to_owned()),
            "#1389 registers the telemetry facade"
        );
        assert!(snapshot.duplicate_public_tool_names.is_empty());
        assert!(snapshot.forbidden_public_tool_names.is_empty());
        assert_eq!(snapshot.over_limit_by, 0);
    }

    #[test]
    fn production_client_public_tools_list_gate_is_schema_safe_and_capped() {
        let tools = super::super::schema_sanitize::sanitize_tools(
            super::super::SynapseService::tool_router().list_all(),
        );
        let mut by_name = std::collections::BTreeMap::new();
        let mut full_tool_names = Vec::new();
        for tool in tools {
            full_tool_names.push(tool.name.to_string());
            by_name.insert(tool.name.to_string(), tool);
        }
        full_tool_names.sort();

        let public_names = public_tool_names();
        validate_public_tool_registry_names(&public_names).expect("public registry <=40");
        assert!(
            public_names.len() <= PUBLIC_TOOL_LIMIT,
            "public registry count {} exceeds {PUBLIC_TOOL_LIMIT}",
            public_names.len()
        );

        let normal_agent_names =
            visible_tool_names_for_profile(ToolProfileKind::NormalAgent, &full_tool_names);
        assert!(
            normal_agent_names.len() <= PUBLIC_TOOL_LIMIT,
            "normal_agent tools/list count {} exceeds {PUBLIC_TOOL_LIMIT}",
            normal_agent_names.len()
        );

        let mut strict_client_errors = Vec::new();
        let mut checked = BTreeSet::new();
        for name in public_names.iter().chain(normal_agent_names.iter()) {
            if !checked.insert(name.clone()) {
                continue;
            }
            let tool = by_name
                .get(name)
                .unwrap_or_else(|| panic!("registered public/default tool missing: {name}"));
            strict_client_errors.extend(strict_client_tool_schema_errors(tool));
        }
        assert!(
            strict_client_errors.is_empty(),
            "production-client tools/list schema gate failed: {strict_client_errors:#?}"
        );
    }

    #[test]
    fn production_client_schema_gate_rejects_boolean_true_subschema() {
        let schema = json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "payload": true
            }
        });
        let mut errors = Vec::new();
        strict_client_schema_errors(&schema, "synthetic_tool.inputSchema", &mut errors);
        assert_eq!(
            errors,
            vec!["synthetic_tool.inputSchema.properties.payload is a bare boolean schema"]
        );
    }

    #[test]
    fn production_client_schema_gate_allows_boolean_defaults_and_additional_properties() {
        let schema = json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "enabled": {
                    "type": "boolean",
                    "default": false
                },
                "metadata": {
                    "type": "object",
                    "additionalProperties": true
                }
            }
        });
        let mut errors = Vec::new();
        strict_client_schema_errors(&schema, "synthetic_tool.inputSchema", &mut errors);
        assert!(errors.is_empty(), "{errors:#?}");
    }

    #[test]
    fn production_client_schema_gate_rejects_invalid_enum_schema() {
        let schema = json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "operation": {
                    "type": "string",
                    "enum": "status"
                }
            }
        });
        let mut errors = Vec::new();
        strict_client_schema_errors(&schema, "synthetic_tool.inputSchema", &mut errors);
        assert_eq!(
            errors,
            vec!["synthetic_tool.inputSchema.properties.operation.enum is not a non-empty array"]
        );
    }

    #[test]
    fn public_tool_registry_rejects_duplicate_public_names() {
        let names = vec!["health".to_owned(), "health".to_owned()];
        let error = validate_public_tool_registry_names(&names).expect_err("duplicate rejected");
        let data = registry_error_data(&error);
        assert_eq!(
            data.get("detail_code").and_then(Value::as_str),
            Some("PUBLIC_TOOL_REGISTRY_INVALID")
        );
        assert_eq!(
            data.get("duplicate_public_tool_names"),
            Some(&json!(["health"]))
        );
    }

    #[test]
    fn public_tool_registry_rejects_forty_first_public_tool() {
        let names = (0..=PUBLIC_TOOL_LIMIT)
            .map(|index| format!("facade_{index:02}"))
            .collect::<Vec<_>>();
        let error = validate_public_tool_registry_names(&names).expect_err("41st tool rejected");
        let data = registry_error_data(&error);
        assert_eq!(
            data.get("detail_code").and_then(Value::as_str),
            Some("PUBLIC_TOOL_REGISTRY_INVALID")
        );
        assert_eq!(data.get("over_limit_by").and_then(Value::as_u64), Some(1));
        assert_eq!(
            data.get("public_tool_count").and_then(Value::as_u64),
            Some(41)
        );
    }

    #[test]
    fn public_tool_registry_rejects_implementation_tool_publication() {
        let names = vec!["health".to_owned(), "cdp_open_tab".to_owned()];
        let error =
            validate_public_tool_registry_names(&names).expect_err("implementation tool rejected");
        let data = registry_error_data(&error);
        assert_eq!(
            data.get("detail_code").and_then(Value::as_str),
            Some("PUBLIC_TOOL_REGISTRY_INVALID")
        );
        assert_eq!(
            data.get("forbidden_public_tool_names"),
            Some(&json!(["cdp_open_tab"]))
        );
    }

    #[test]
    fn facade_contract_covers_every_public_tool_with_readback_rules() {
        let public_names = public_tool_names();
        let snapshot = facade_contract_snapshot_for(&public_names).expect("facade contract");
        assert_eq!(snapshot.public_tool_count, PUBLIC_TOOL_LIMIT);
        assert_eq!(snapshot.contract_tool_count, PUBLIC_TOOL_LIMIT);
        assert_eq!(snapshot.missing_contract_tool_names, Vec::<String>::new());
        assert_eq!(snapshot.unknown_contract_tool_names, Vec::<String>::new());
        assert_eq!(snapshot.duplicate_contract_tool_names, Vec::<String>::new());
        assert_eq!(snapshot.duplicate_operation_names, Vec::<String>::new());
        assert_eq!(snapshot.invalid_contract_reasons, Vec::<String>::new());
        assert!(snapshot.operation_count > PUBLIC_TOOL_LIMIT);
        assert!(snapshot.mutating_operation_count > 0);
        assert!(snapshot.contracts.iter().any(|contract| {
            contract.tool_name == "browser_tabs"
                && contract.operation_enum == "BrowserTabsOperation"
                && contract.operations.iter().any(|operation| {
                    operation.operation == "select"
                        && operation.mutates_state
                        && operation.readback_source_of_truth.is_some()
                })
        }));
        assert!(snapshot.contracts.iter().all(|contract| {
            contract.operations.iter().all(|operation| {
                !operation.mutates_state || operation.readback_source_of_truth.is_some()
            })
        }));
    }

    /// Recursively collect every `enum`/`const` string reachable from an
    /// `operation` schema node, resolving `$ref` into the schema-root `$defs`
    /// and descending `oneOf`/`anyOf` (covers schemars enum shapes:
    /// inline `enum`, `$ref` to a `$defs` enum, a `$defs` `oneOf` of `const`
    /// variants with per-variant docs, and the `Option<Enum>` `anyOf[ref,null]`
    /// shape). This mirrors what an MCP client actually parses from the wire.
    fn collect_operation_enum_consts(root: &Value, node: &Value, out: &mut BTreeSet<String>) {
        if let Some(values) = node.get("enum").and_then(Value::as_array) {
            for value in values {
                if let Some(text) = value.as_str() {
                    out.insert(text.to_owned());
                }
            }
        }
        if let Some(text) = node.get("const").and_then(Value::as_str) {
            out.insert(text.to_owned());
        }
        for combinator in ["oneOf", "anyOf", "allOf"] {
            if let Some(variants) = node.get(combinator).and_then(Value::as_array) {
                for variant in variants {
                    collect_operation_enum_consts(root, variant, out);
                }
            }
        }
        if let Some(reference) = node.get("$ref").and_then(Value::as_str) {
            if let Some(name) = reference.strip_prefix("#/$defs/") {
                if let Some(def) = root.get("$defs").and_then(|defs| defs.get(name)) {
                    collect_operation_enum_consts(root, def, out);
                }
            }
        }
    }

    /// The set of operation wire-names the built tool schema exposes, or `None`
    /// when the tool has no `operation` property (a flat single-purpose facade).
    fn schema_operation_names(schema: &Value) -> Option<BTreeSet<String>> {
        let operation = schema.get("properties")?.get("operation")?;
        let mut out = BTreeSet::new();
        collect_operation_enum_consts(schema, operation, &mut out);
        Some(out)
    }

    /// Pure comparison of the vendored contracts against a map of built tool
    /// input schemas, returning one human-readable failure per drift. Kept as a
    /// standalone function so both the live-surface gate and the synthetic
    /// non-vacuity test below exercise the exact same detection logic.
    fn facade_contract_schema_failures(
        contracts: &[FacadeToolContractSpec],
        schema_by_name: &std::collections::BTreeMap<String, Value>,
    ) -> Vec<String> {
        let mut failures = Vec::new();
        for contract in contracts {
            let Some(schema) = schema_by_name.get(contract.tool_name) else {
                failures.push(format!(
                    "{}: contract present but tool missing from built surface",
                    contract.tool_name
                ));
                continue;
            };
            let contract_ops = contract
                .operations
                .iter()
                .map(|operation| operation.operation.to_owned())
                .collect::<BTreeSet<String>>();
            match schema_operation_names(schema) {
                None => {
                    if contract.operation_enum != FLAT_FACADE_OPERATION_ENUM {
                        failures.push(format!(
                            "{}: tool has NO `operation` property but contract operation_enum={:?}; flat facades must use FLAT_FACADE_OPERATION_ENUM",
                            contract.tool_name, contract.operation_enum
                        ));
                    }
                }
                Some(schema_ops) => {
                    if contract.operation_enum == FLAT_FACADE_OPERATION_ENUM {
                        failures.push(format!(
                            "{}: contract marked flat but tool exposes operation enum {schema_ops:?}",
                            contract.tool_name
                        ));
                    } else if schema_ops != contract_ops {
                        let in_schema_only = schema_ops
                            .difference(&contract_ops)
                            .cloned()
                            .collect::<Vec<_>>();
                        let in_contract_only = contract_ops
                            .difference(&schema_ops)
                            .cloned()
                            .collect::<Vec<_>>();
                        failures.push(format!(
                            "{}: operation drift — schema-only={in_schema_only:?} contract-only={in_contract_only:?}",
                            contract.tool_name
                        ));
                    }
                }
            }
        }
        failures
    }

    fn built_tool_schema_map() -> std::collections::BTreeMap<String, Value> {
        crate::server::schema_sanitize::sanitize_tools(
            crate::server::SynapseService::tool_router().list_all(),
        )
        .iter()
        .map(|tool| {
            (
                tool.name.as_ref().to_owned(),
                Value::Object((*tool.input_schema).clone()),
            )
        })
        .collect()
    }

    // ROOT-CAUSE GATE (metadata drift): the structural validators above prove
    // every public tool HAS a contract, but never proved the contract's
    // operation list matches the tool's real serialized `operation` enum. That
    // gap let `browser_storage` ship a contract declaring read/write while the
    // tool actually exposed get/set/clear/save_state/load_state, and let flat
    // tools (health/observe/find/read_text/subscribe) name `*Operation` enums
    // that never existed. This gate closes it by deriving the operation set from
    // the real built tool surface (the same schema an MCP client parses) and
    // asserting exact parity with FACADE_TOOL_CONTRACTS — the vendored,
    // code-reviewed source of truth. Adding/removing/renaming a facade operation
    // now fails here until the contract is updated in the same change.
    #[test]
    fn facade_contract_operations_match_live_schema() {
        let schema_by_name = built_tool_schema_map();
        let failures = facade_contract_schema_failures(FACADE_TOOL_CONTRACTS, &schema_by_name);
        assert!(
            failures.is_empty(),
            "FACADE_TOOL_CONTRACTS drifted from the built tool schema:\n{}",
            failures.join("\n")
        );
    }

    // Non-vacuity guard: prove the gate above actually detects the two drift
    // classes it exists to catch, using the REAL built schemas but deliberately
    // wrong contracts (the exact historical bugs). If the detector ever silently
    // stops flagging drift, this test fails instead of the real gate passing for
    // the wrong reason.
    #[test]
    fn facade_contract_schema_gate_detects_known_drift() {
        let schema_by_name = built_tool_schema_map();

        // Class 1: an enum-backed tool whose contract lists the wrong ops.
        const WRONG_OPS: &[FacadeOperationContractSpec] = &[
            op(
                "read",
                false,
                true,
                "wrong",
                None,
                error_codes::ACTION_TARGET_INVALID,
                "wrong",
            ),
            op(
                "write",
                true,
                true,
                "wrong",
                Some("wrong"),
                error_codes::TOOL_PARAMS_INVALID,
                "wrong",
            ),
        ];
        const WRONG_ENUM_CONTRACT: &[FacadeToolContractSpec] = &[facade_contract(
            "browser_storage",
            "BrowserStorageOperation",
            "wrong",
            WRONG_OPS,
        )];
        let failures = facade_contract_schema_failures(WRONG_ENUM_CONTRACT, &schema_by_name);
        assert_eq!(failures.len(), 1, "{failures:?}");
        assert!(
            failures[0].contains("browser_storage")
                && failures[0].contains("operation drift")
                && failures[0].contains("save_state"),
            "{failures:?}"
        );

        // Class 2: a flat tool whose contract fabricates a non-existent enum.
        const FLAT_OPS: &[FacadeOperationContractSpec] = &[op(
            "status",
            false,
            false,
            "wrong",
            None,
            error_codes::TOOL_INTERNAL_ERROR,
            "wrong",
        )];
        const FABRICATED_FLAT_CONTRACT: &[FacadeToolContractSpec] = &[facade_contract(
            "health",
            "HealthOperation",
            "wrong",
            FLAT_OPS,
        )];
        let failures = facade_contract_schema_failures(FABRICATED_FLAT_CONTRACT, &schema_by_name);
        assert_eq!(failures.len(), 1, "{failures:?}");
        assert!(
            failures[0].contains("health")
                && failures[0].contains("NO `operation`")
                && failures[0].contains("FLAT_FACADE_OPERATION_ENUM"),
            "{failures:?}"
        );

        // Correctly-marked flat tool must NOT be flagged.
        const CORRECT_FLAT_CONTRACT: &[FacadeToolContractSpec] = &[facade_contract(
            "health",
            FLAT_FACADE_OPERATION_ENUM,
            "ok",
            FLAT_OPS,
        )];
        assert!(facade_contract_schema_failures(CORRECT_FLAT_CONTRACT, &schema_by_name).is_empty());
    }

    #[test]
    fn facade_contract_rejects_unknown_contract_tool() {
        const OPS: &[FacadeOperationContractSpec] = &[op(
            "status",
            false,
            false,
            "test SoT",
            None,
            error_codes::TOOL_INTERNAL_ERROR,
            "fix test",
        )];
        const CONTRACTS: &[FacadeToolContractSpec] = &[
            facade_contract("health", "HealthOperation", "test SoT", OPS),
            facade_contract("not_public", "NotPublicOperation", "test SoT", OPS),
        ];
        let error = validate_facade_contracts(&["health".to_owned()], CONTRACTS)
            .expect_err("unknown public contract rejected");
        let data = registry_error_data(&error);
        assert_eq!(
            data.get("detail_code").and_then(Value::as_str),
            Some(FACADE_CONTRACT_ERROR_CODE)
        );
        assert_eq!(
            data.get("unknown_contract_tool_names"),
            Some(&json!(["not_public"]))
        );
    }

    #[test]
    fn facade_contract_rejects_duplicate_operations() {
        const OPS: &[FacadeOperationContractSpec] = &[
            op(
                "status",
                false,
                false,
                "test SoT",
                None,
                error_codes::TOOL_INTERNAL_ERROR,
                "fix test",
            ),
            op(
                "status",
                false,
                false,
                "test SoT",
                None,
                error_codes::TOOL_INTERNAL_ERROR,
                "fix test",
            ),
        ];
        const CONTRACTS: &[FacadeToolContractSpec] = &[facade_contract(
            "health",
            "HealthOperation",
            "test SoT",
            OPS,
        )];
        let error = validate_facade_contracts(&["health".to_owned()], CONTRACTS)
            .expect_err("duplicate operation rejected");
        let data = registry_error_data(&error);
        assert_eq!(
            data.get("detail_code").and_then(Value::as_str),
            Some(FACADE_CONTRACT_ERROR_CODE)
        );
        assert_eq!(
            data.get("duplicate_operation_names"),
            Some(&json!(["health.status"]))
        );
    }

    #[test]
    fn facade_contract_rejects_mutation_without_readback_source() {
        const OPS: &[FacadeOperationContractSpec] = &[op(
            "set",
            true,
            false,
            "test SoT",
            None,
            error_codes::STORAGE_WRITE_FAILED,
            "fix test",
        )];
        const CONTRACTS: &[FacadeToolContractSpec] = &[facade_contract(
            "health",
            "HealthOperation",
            "test SoT",
            OPS,
        )];
        let error = validate_facade_contracts(&["health".to_owned()], CONTRACTS)
            .expect_err("mutating operation without readback rejected");
        let data = registry_error_data(&error);
        assert_eq!(
            data.get("detail_code").and_then(Value::as_str),
            Some(FACADE_CONTRACT_ERROR_CODE)
        );
        let reasons = data
            .get("invalid_contract_reasons")
            .and_then(Value::as_array)
            .expect("invalid reasons");
        assert!(reasons.iter().any(|reason| {
            reason
                .as_str()
                .is_some_and(|reason| reason.contains("mutates_state requires readback"))
        }));
    }

    #[test]
    fn codex_restart_handoff_readback_marks_candidate_pid_mismatch() {
        let dir = TempDir::new().expect("tmp");
        let path = dir
            .path()
            .join("codex-restart-handoff-1234-20260701T000000000Z.json");
        let live_pid = std::process::id();
        let candidate_pid = if live_pid == u32::MAX {
            live_pid - 1
        } else {
            live_pid + 1
        };
        let record = json!({
            "schema_version": 2,
            "artifact_kind": "synapse_codex_restart_handoff",
            "created_at_utc": "2026-07-01T00:00:00Z",
            "reason_code": "SYNAPSE_CODEX_CURRENT_PROCESS_SCHEMA_STALE",
            "reason": "test",
            "phase": "pre_handoff_candidate",
            "required_restart": true,
            "no_in_process_hot_refresh": true,
            "codex_process": {
                "pid": 1234,
                "name": "codex.exe",
                "command_line": "codex resume --yolo"
            },
            "daemon": {
                "bind": "127.0.0.1:7700",
                "pid": candidate_pid,
                "pid_role": "preflight_candidate",
                "pid_authoritative_for_configured_bind": false,
                "tool_count": 40,
                "tool_surface_sha256": "test-surface",
                "snapshot_path": null
            },
            "current_process_start_surface": {
                "snapshot_status": "readable",
                "env_hash": "test-surface",
                "env_snapshot_path": "C:\\Users\\hotra\\AppData\\Local\\synapse\\codex-start-snapshots\\test.json"
            },
            "active_issue": {
                "issue_ref": "#1471"
            }
        });
        std::fs::write(
            &path,
            serde_json::to_vec_pretty(&record).expect("serialize handoff"),
        )
        .expect("write handoff");

        let readback = codex_restart_handoff_readback(&path);

        assert_eq!(readback.daemon_pid, Some(candidate_pid));
        assert_eq!(readback.daemon_bind.as_deref(), Some("127.0.0.1:7700"));
        assert_eq!(
            readback.daemon_pid_role.as_deref(),
            Some("preflight_candidate")
        );
        assert_eq!(
            readback.daemon_pid_authoritative_for_configured_bind,
            Some(false)
        );
        assert_eq!(
            readback.current_process_start_snapshot_status.as_deref(),
            Some("readable")
        );
        assert_eq!(
            readback.current_process_start_env_hash.as_deref(),
            Some("test-surface")
        );
        assert_eq!(readback.live_daemon_pid, live_pid);
        assert_eq!(readback.daemon_pid_matches_live_daemon, Some(false));
        assert!(
            readback
                .daemon_pid_mismatch_detail
                .as_deref()
                .is_some_and(|detail| detail.contains("pre_handoff_candidate")
                    && detail.contains("preflight_candidate"))
        );
    }

    fn test_host_surface(hash: Option<&str>) -> CodexToolSurfaceSnapshotReadback {
        CodexToolSurfaceSnapshotReadback {
            path: "host.json".to_owned(),
            exists: hash.is_some(),
            len_bytes: None,
            sha256: None,
            read_error: None,
            tool_count: Some(40),
            tool_surface_sha256: hash.map(ToOwned::to_owned),
            tool_names: names()
                .into_iter()
                .take(PUBLIC_TOOL_LIMIT)
                .collect::<Vec<_>>(),
        }
    }

    fn test_restart_handoff(
        required_restart: bool,
        start_hash: Option<&str>,
    ) -> CodexRestartHandoffReadback {
        CodexRestartHandoffReadback {
            path: "handoff.json".to_owned(),
            exists: true,
            len_bytes: None,
            sha256: None,
            read_error: None,
            created_at_utc: Some("2026-07-02T00:00:00Z".to_owned()),
            reason_code: Some("SYNAPSE_CODEX_CURRENT_PROCESS_SCHEMA_STALE".to_owned()),
            reason: Some("test".to_owned()),
            phase: Some("post_handoff_current_daemon".to_owned()),
            required_restart,
            no_in_process_hot_refresh: required_restart,
            stale_codex_pid: Some(std::process::id()),
            stale_codex_command_line: Some("codex resume --yolo".to_owned()),
            active_issue_ref: Some("#1488".to_owned()),
            daemon_pid: Some(1),
            daemon_bind: Some("127.0.0.1:7700".to_owned()),
            daemon_pid_role: Some("installed_configured_daemon".to_owned()),
            daemon_pid_authoritative_for_configured_bind: Some(true),
            daemon_tool_count: Some(40),
            daemon_tool_surface_sha256: Some("historical-surface".to_owned()),
            current_process_start_snapshot_status: Some(if start_hash.is_some() {
                "readable".to_owned()
            } else {
                "missing_env".to_owned()
            }),
            current_process_start_env_hash: start_hash.map(ToOwned::to_owned),
            live_daemon_pid: std::process::id(),
            daemon_pid_matches_live_daemon: Some(false),
            daemon_pid_mismatch_detail: Some("historical daemon pid".to_owned()),
        }
    }

    #[test]
    fn restart_handoff_matching_current_start_hash_is_not_actionable() {
        let host = test_host_surface(Some(
            "594f2abc0412c9f6c87d7c1eba9c8f1eaeb7100a542f1002768b44b77d71e6fb",
        ));
        let handoff = test_restart_handoff(
            true,
            Some("sha256:594F2ABC0412C9F6C87D7C1EBA9C8F1EAEB7100A542F1002768B44B77D71E6FB"),
        );

        assert!(!restart_handoff_requires_current_codex_restart(
            &handoff, &host
        ));
    }

    #[test]
    fn restart_handoff_different_current_start_hash_is_actionable() {
        let host = test_host_surface(Some(
            "594f2abc0412c9f6c87d7c1eba9c8f1eaeb7100a542f1002768b44b77d71e6fb",
        ));
        let handoff = test_restart_handoff(
            true,
            Some("fd2ee96bcb5a04bc7a0a53d2559713cfdd698d390bfb182521413bcf4954973d"),
        );

        assert!(restart_handoff_requires_current_codex_restart(
            &handoff, &host
        ));
    }

    #[test]
    fn restart_handoff_missing_current_start_hash_fails_closed() {
        let host = test_host_surface(Some(
            "594f2abc0412c9f6c87d7c1eba9c8f1eaeb7100a542f1002768b44b77d71e6fb",
        ));
        let handoff = test_restart_handoff(true, None);

        assert!(restart_handoff_requires_current_codex_restart(
            &handoff, &host
        ));
    }

    #[test]
    fn restart_handoff_readback_clears_current_action_when_start_hash_matches() {
        let mut handoff = test_restart_handoff(
            true,
            Some("sha256:594F2ABC0412C9F6C87D7C1EBA9C8F1EAEB7100A542F1002768B44B77D71E6FB"),
        );

        resolve_restart_handoff_current_action_readback(&mut handoff, true, false, true);

        assert!(!handoff.required_restart);
        assert!(!handoff.no_in_process_hot_refresh);
    }

    #[test]
    fn restart_handoff_readback_clears_current_action_when_stale_pid_dead() {
        let mut handoff = test_restart_handoff(
            true,
            Some("fd2ee96bcb5a04bc7a0a53d2559713cfdd698d390bfb182521413bcf4954973d"),
        );

        resolve_restart_handoff_current_action_readback(&mut handoff, true, true, false);

        assert!(!handoff.required_restart);
        assert!(!handoff.no_in_process_hot_refresh);
    }

    #[test]
    fn restart_handoff_readback_keeps_action_for_live_stale_codex_pid() {
        let mut handoff = test_restart_handoff(
            true,
            Some("fd2ee96bcb5a04bc7a0a53d2559713cfdd698d390bfb182521413bcf4954973d"),
        );

        resolve_restart_handoff_current_action_readback(&mut handoff, true, true, true);

        assert!(handoff.required_restart);
        assert!(handoff.no_in_process_hot_refresh);
    }

    #[test]
    fn restart_handoff_readback_keeps_action_when_host_snapshot_not_current() {
        let mut handoff = test_restart_handoff(
            true,
            Some("fd2ee96bcb5a04bc7a0a53d2559713cfdd698d390bfb182521413bcf4954973d"),
        );

        resolve_restart_handoff_current_action_readback(&mut handoff, false, true, false);

        assert!(handoff.required_restart);
        assert!(handoff.no_in_process_hot_refresh);
    }

    #[test]
    fn non_required_restart_handoff_is_not_actionable() {
        let host = test_host_surface(None);
        let handoff = test_restart_handoff(false, None);

        assert!(!restart_handoff_requires_current_codex_restart(
            &handoff, &host
        ));
    }

    #[test]
    fn normal_profile_routes_foreground_capability_without_raw_primitives() {
        let visible = visible_tool_names_for_profile(ToolProfileKind::NormalAgent, &names());
        assert_public_facade_surface(&visible);
        assert!(!visible.contains(&"act_run_shell".to_owned()));
        assert!(!visible.contains(&"act_run_shell_start".to_owned()));
        assert!(!visible.contains(&"act_run_shell_status".to_owned()));
        assert!(!visible.contains(&"act_run_shell_cancel".to_owned()));
        assert!(!visible.contains(&"act_launch".to_owned()));
        assert!(!visible.contains(&"approval_list".to_owned()));
        assert!(!visible.contains(&"approval_gate".to_owned()));
        assert!(!visible.contains(&"approval_decide".to_owned()));
        assert!(!visible.contains(&"escalation_list".to_owned()));
        assert!(!visible.contains(&"escalation_config_set".to_owned()));
        assert!(!visible.contains(&"escalation_ack".to_owned()));
        assert!(!visible.contains(&"timeline_get".to_owned()));
        assert!(!visible.contains(&"timeline_search".to_owned()));
        assert!(!visible.contains(&"timeline_stats".to_owned()));
        assert!(!visible.contains(&"timeline_pause".to_owned()));
        assert!(!visible.contains(&"timeline_resume".to_owned()));
        assert!(!visible.contains(&"timeline_exclusions".to_owned()));
        assert!(!visible.contains(&"timeline_purge".to_owned()));
        assert!(!visible.contains(&"timeline_redact".to_owned()));
        assert!(!visible.contains(&"timeline_digest".to_owned()));
        assert!(!visible.contains(&"episode_list".to_owned()));
        assert!(!visible.contains(&"episode_get".to_owned()));
        assert!(!visible.contains(&"episode_segment".to_owned()));
        assert!(!visible.contains(&"routine_mine".to_owned()));
        assert!(!visible.contains(&"routine_list".to_owned()));
        assert!(!visible.contains(&"routine_inspect".to_owned()));
        assert!(!visible.contains(&"routine_update".to_owned()));
        assert!(!visible.contains(&"routine_feedback".to_owned()));
        assert!(!visible.contains(&"routine_label_export".to_owned()));
        assert!(!visible.contains(&"routine_automate".to_owned()));
        assert!(!visible.contains(&"armed_routine_tick".to_owned()));
        assert!(!visible.contains(&"intent_current".to_owned()));
        assert!(!visible.contains(&"intent_detect_tick".to_owned()));
        assert!(!visible.contains(&"suggestion_tick".to_owned()));
        assert!(!visible.contains(&"suggestion_list".to_owned()));
        assert!(!visible.contains(&"suggestion_accept".to_owned()));
        assert!(!visible.contains(&"reality_baseline".to_owned()));
        assert!(!visible.contains(&"observe_delta".to_owned()));
        assert!(!visible.contains(&"reality_audit".to_owned()));
        assert!(!visible.contains(&"verification_inbox".to_owned()));
        assert!(!visible.contains(&"verification_poll".to_owned()));
        assert!(!visible.contains(&"verification_audit".to_owned()));
        assert!(!visible.contains(&"verification_bind".to_owned()));
        assert!(!visible.contains(&"verification_sources".to_owned()));
        assert!(!visible.contains(&"cdp_activate_tab".to_owned()));
        assert!(!visible.contains(&"cdp_close_tab".to_owned()));
        assert!(!visible.contains(&"cdp_navigate_tab".to_owned()));
        assert!(!visible.contains(&"cdp_open_tab".to_owned()));
        assert!(!visible.contains(&"cdp_target_info".to_owned()));
        assert!(!visible.contains(&"target_act".to_owned()));
        assert!(!visible.contains(&"browser_content".to_owned()));
        assert!(!visible.contains(&"browser_locate".to_owned()));
        assert!(!visible.contains(&"browser_set_value".to_owned()));
        assert!(!visible.contains(&"tool_profile_set".to_owned()));
        assert!(!visible.contains(&"tool_profile_status".to_owned()));
        assert!(!visible.contains(&"act_click".to_owned()));
        assert!(!visible.contains(&"act_type".to_owned()));
        assert!(!visible.contains(&"release_all".to_owned()));
        assert!(visible.contains(&"browser_debugger".to_owned()));
        assert_debugger_only_hidden(&visible);

        let policy = foreground_capability_policy(ToolProfileKind::NormalAgent);
        assert!(policy.profile_preserves_capability);
        assert!(
            policy
                .agent_logical_foreground
                .contains("per-session foreground-equivalent")
        );
        assert!(
            policy
                .human_os_foreground
                .contains("never an implicit fallback")
        );

        let routes = hidden_tool_capability_routes(&visible);
        assert!(
            !routes.iter().any(|route| route.hidden_tool == "act_launch"),
            "act_launch is a launch/target-creation capability with its own policy checks, not a hidden foreground input primitive"
        );
        let act_type_route = routes
            .iter()
            .find(|route| route.hidden_tool == "act_type")
            .expect("act_type route");
        assert!(
            act_type_route
                .preferred_tools
                .contains(&"act operation=invoke set_field".to_owned())
        );
        assert!(
            act_type_route
                .preferred_tools
                .contains(&"browser_form operation=set_value".to_owned())
        );
        assert!(
            act_type_route
                .preferred_tools
                .contains(&"browser_dom operation=locate".to_owned())
        );
        let browser_debugger_route = routes
            .iter()
            .find(|route| route.hidden_tool == "browser_console_messages")
            .expect("browser_console_messages route");
        assert!(
            browser_debugger_route
                .preferred_tools
                .iter()
                .any(|tool| tool.contains("profile=browser_debugger"))
        );
        assert!(
            browser_debugger_route
                .preferred_tools
                .contains(&"browser_debugger operation=<matching operation>".to_owned())
        );
        assert!(
            act_type_route
                .agent_logical_foreground_policy
                .contains("agent_logical_foreground")
        );
        assert!(
            act_type_route
                .human_os_foreground_policy
                .contains("never use the human OS foreground")
        );
    }

    #[test]
    fn browser_profile_uses_public_facade_surface() {
        let visible = visible_tool_names_for_profile(ToolProfileKind::BrowserControl, &names());
        assert_public_facade_surface(&visible);
        assert!(visible.contains(&"browser_tabs".to_owned()));
        assert!(visible.contains(&"browser_nav".to_owned()));
        assert!(visible.contains(&"browser_wait".to_owned()));
        assert!(visible.contains(&"browser_storage".to_owned()));
        assert!(!visible.contains(&"act_run_shell".to_owned()));
        assert!(!visible.contains(&"act_click".to_owned()));
        assert_debugger_only_hidden(&visible);
    }

    #[test]
    fn browser_debugger_profile_uses_public_facade_surface_without_raw_tools() {
        let visible = visible_tool_names_for_profile(ToolProfileKind::BrowserDebugger, &names());
        assert_public_facade_surface(&visible);
        assert!(visible.contains(&"browser_debugger".to_owned()));
        assert!(visible.contains(&"browser_capture".to_owned()));
        assert!(visible.contains(&"browser_dom".to_owned()));
        assert!(visible.contains(&"browser_form".to_owned()));
        assert!(visible.contains(&"browser_wait".to_owned()));
        assert!(visible.contains(&"profile".to_owned()));
        assert!(visible.contains(&"telemetry".to_owned()));
        assert!(!visible.contains(&"act_run_shell".to_owned()));
        assert!(!visible.contains(&"act_spawn_agent".to_owned()));
        assert!(!visible.contains(&"act_click".to_owned()));
        assert!(!visible.contains(&"act_type".to_owned()));
        assert!(!visible.contains(&"release_all".to_owned()));

        let policy = foreground_capability_policy(ToolProfileKind::BrowserDebugger);
        assert!(policy.profile_preserves_capability);
        assert!(policy.preferred_path.contains("browser_debugger facade"));
        assert!(
            policy
                .preferred_path
                .contains("enabled only by this explicit profile")
        );
    }

    #[test]
    fn break_glass_profile_uses_public_facade_surface() {
        let visible = visible_tool_names_for_profile(ToolProfileKind::BreakGlass, &names());
        assert_public_facade_surface(&visible);
        assert_eq!(
            denied_break_glass_tools(&visible),
            BREAK_GLASS_HAZARDOUS_TOOLS
                .iter()
                .map(|name| (*name).to_owned())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn break_glass_requires_confirm_reason_and_lease() {
        let proof = ToolProfileLeaseProof {
            required: true,
            held: false,
            owner_session_id: None,
            caller_is_owner: false,
            expires_in_ms: None,
        };
        assert!(
            validate_profile_set_policy(
                "s1",
                ToolProfileKind::BreakGlass,
                Some("need raw foreground click"),
                false,
                &proof,
            )
            .is_err()
        );
        assert!(
            validate_profile_set_policy("s1", ToolProfileKind::BreakGlass, None, true, &proof,)
                .is_err()
        );
        assert!(
            validate_profile_set_policy(
                "s1",
                ToolProfileKind::BreakGlass,
                Some("need raw foreground click"),
                true,
                &proof,
            )
            .is_err()
        );
    }

    #[test]
    fn break_glass_policy_accepts_owned_lease_proof() {
        let proof = ToolProfileLeaseProof {
            required: true,
            held: true,
            owner_session_id: Some("s1".to_owned()),
            caller_is_owner: true,
            expires_in_ms: Some(10_000),
        };
        validate_profile_set_policy(
            "s1",
            ToolProfileKind::BreakGlass,
            Some("need raw foreground click"),
            true,
            &proof,
        )
        .expect("owned lease proof should allow break-glass");
    }

    #[test]
    fn reality_write_grant_gate_requires_confirm_reason_and_lease() {
        // #1559: gated exactly like break_glass — confirm + reason + owned lease.
        let unowned = ToolProfileLeaseProof {
            required: true,
            held: false,
            owner_session_id: None,
            caller_is_owner: false,
            expires_in_ms: None,
        };
        // Missing confirm.
        assert!(
            validate_reality_write_grant_gate("s1", Some("need reality write"), false, &unowned)
                .is_err()
        );
        // Missing reason.
        assert!(validate_reality_write_grant_gate("s1", None, true, &unowned).is_err());
        // Missing lease ownership.
        let lease_err =
            validate_reality_write_grant_gate("s1", Some("need reality write"), true, &unowned)
                .expect_err("grant without owned lease must be rejected");
        assert_eq!(
            lease_err
                .data
                .as_ref()
                .and_then(|data| data.get("code"))
                .and_then(Value::as_str),
            Some(error_codes::ACTION_FOREGROUND_LEASE_NOT_HELD)
        );

        // All three satisfied → accepted.
        let owned = ToolProfileLeaseProof {
            required: true,
            held: true,
            owner_session_id: Some("s1".to_owned()),
            caller_is_owner: true,
            expires_in_ms: Some(10_000),
        };
        validate_reality_write_grant_gate("s1", Some("need reality write"), true, &owned)
            .expect("confirm+reason+owned-lease should pass the reality-write grant gate");
        println!("readback=gate reality_write_grant confirm+reason+lease accepted");
    }

    #[test]
    fn browser_debugger_requires_confirm_and_reason_but_not_foreground_lease() {
        let proof = ToolProfileLeaseProof {
            required: false,
            held: false,
            owner_session_id: None,
            caller_is_owner: false,
            expires_in_ms: None,
        };
        assert!(
            validate_profile_set_policy(
                "s1",
                ToolProfileKind::BrowserDebugger,
                Some("inspect raw CDP console messages"),
                false,
                &proof,
            )
            .is_err()
        );
        assert!(
            validate_profile_set_policy(
                "s1",
                ToolProfileKind::BrowserDebugger,
                None,
                true,
                &proof,
            )
            .is_err()
        );
        validate_profile_set_policy(
            "s1",
            ToolProfileKind::BrowserDebugger,
            Some("inspect raw CDP console messages"),
            true,
            &proof,
        )
        .expect("browser_debugger requires explicit reason/confirm but not OS foreground lease");
    }

    #[test]
    fn default_normal_profile_persists_policy_row_and_filters_tools() {
        let dir = TempDir::new().expect("tmp");
        let service = service_with_db(dir.path());
        let session_id = "issue1008-default-session";
        let before = service
            .read_tool_profile_assignment(session_id)
            .expect("read before");
        assert!(before.is_none());

        let tools = tool_names(
            service
                .tools_for_session_profile(Some(session_id))
                .expect("profile tools"),
        );
        let registry =
            public_tool_registry_snapshot_for(&service.full_tool_names()).expect("registry");
        assert_eq!(tools, registry.registered_tools_present);
        assert!(tools.contains(&"health".to_owned()));
        assert!(tools.contains(&"profile".to_owned()));
        assert!(tools.contains(&"session".to_owned()));
        assert!(tools.contains(&"subscribe".to_owned()));
        assert!(tools.contains(&"browser_tabs".to_owned()));
        assert!(tools.contains(&"browser_nav".to_owned()));
        assert!(tools.contains(&"browser_dom".to_owned()));
        assert!(tools.contains(&"browser_form".to_owned()));
        assert!(tools.contains(&"browser_wait".to_owned()));
        assert!(tools.contains(&"browser_capture".to_owned()));
        assert!(tools.contains(&"browser_storage".to_owned()));
        assert!(tools.contains(&"browser_debugger".to_owned()));
        assert!(tools.contains(&"shell".to_owned()));
        assert!(tools.contains(&"process".to_owned()));
        assert!(tools.contains(&"timeline".to_owned()));
        assert!(tools.contains(&"episode".to_owned()));
        assert!(tools.contains(&"privacy".to_owned()));
        assert!(tools.contains(&"routine".to_owned()));
        assert!(tools.contains(&"assist".to_owned()));
        assert!(tools.contains(&"reality".to_owned()));
        assert!(tools.contains(&"verification".to_owned()));
        assert!(tools.contains(&"storage".to_owned()));
        assert!(tools.contains(&"model".to_owned()));
        assert!(tools.contains(&"cost".to_owned()));
        assert!(tools.contains(&"hygiene".to_owned()));
        assert!(tools.contains(&"setup".to_owned()));
        assert!(tools.contains(&"telemetry".to_owned()));
        assert!(!tools.contains(&"agent_spawn_task_started".to_owned()));
        assert!(!tools.contains(&"cdp_activate_tab".to_owned()));
        assert!(!tools.contains(&"cdp_close_tab".to_owned()));
        assert!(!tools.contains(&"cdp_navigate_tab".to_owned()));
        assert!(!tools.contains(&"cdp_open_tab".to_owned()));
        assert!(!tools.contains(&"cdp_target_info".to_owned()));
        assert!(!tools.contains(&"act_run_shell".to_owned()));
        assert!(!tools.contains(&"act_run_shell_start".to_owned()));
        assert!(!tools.contains(&"act_run_shell_status".to_owned()));
        assert!(!tools.contains(&"act_run_shell_cancel".to_owned()));
        assert!(!tools.contains(&"act_launch".to_owned()));
        assert!(!tools.contains(&"suggestion_tick".to_owned()));
        assert!(!tools.contains(&"tool_profile_status".to_owned()));
        assert!(!tools.contains(&"tool_profile_set".to_owned()));
        assert!(!tools.contains(&"demo_record_start".to_owned()));
        assert!(!tools.contains(&"profile_authoring_generate".to_owned()));
        assert!(!tools.contains(&"storage_inspect".to_owned()));
        assert!(!tools.contains(&"storage_put_probe_rows".to_owned()));
        assert!(!tools.contains(&"storage_gc_once".to_owned()));
        assert!(!tools.contains(&"storage_pressure_sample".to_owned()));
        assert!(!tools.contains(&"local_model_list".to_owned()));
        assert!(!tools.contains(&"local_model_probe".to_owned()));
        assert!(!tools.contains(&"local_model_register".to_owned()));
        assert!(!tools.contains(&"local_model_update".to_owned()));
        assert!(!tools.contains(&"local_model_remove".to_owned()));
        assert!(!tools.contains(&"hygiene_scan_text".to_owned()));
        assert!(!tools.contains(&"hygiene_scan_storage".to_owned()));
        assert!(!tools.contains(&"hygiene_flags".to_owned()));
        assert!(!tools.contains(&"hygiene_report".to_owned()));
        assert!(!tools.contains(&"agent_cost".to_owned()));
        assert!(!tools.contains(&"agent_cost_price_put".to_owned()));
        assert!(!tools.contains(&"agent_cost_price_list".to_owned()));
        assert!(!tools.contains(&"agent_cost_price_delete".to_owned()));
        assert!(!tools.contains(&"timeline_get".to_owned()));
        assert!(!tools.contains(&"timeline_search".to_owned()));
        assert!(!tools.contains(&"timeline_stats".to_owned()));
        assert!(!tools.contains(&"timeline_pause".to_owned()));
        assert!(!tools.contains(&"timeline_resume".to_owned()));
        assert!(!tools.contains(&"timeline_exclusions".to_owned()));
        assert!(!tools.contains(&"timeline_purge".to_owned()));
        assert!(!tools.contains(&"timeline_redact".to_owned()));
        assert!(!tools.contains(&"episode_list".to_owned()));
        assert!(!tools.contains(&"episode_get".to_owned()));
        assert!(!tools.contains(&"routine_mine".to_owned()));
        assert!(!tools.contains(&"routine_list".to_owned()));
        assert!(!tools.contains(&"routine_inspect".to_owned()));
        assert!(!tools.contains(&"routine_update".to_owned()));
        assert!(!tools.contains(&"routine_feedback".to_owned()));
        assert!(!tools.contains(&"routine_label_export".to_owned()));
        assert!(!tools.contains(&"routine_automate".to_owned()));
        assert!(!tools.contains(&"armed_routine_tick".to_owned()));
        assert!(!tools.contains(&"intent_current".to_owned()));
        assert!(!tools.contains(&"intent_detect_tick".to_owned()));
        assert!(!tools.contains(&"suggestion_tick".to_owned()));
        assert!(!tools.contains(&"suggestion_list".to_owned()));
        assert!(!tools.contains(&"suggestion_accept".to_owned()));
        assert!(!tools.contains(&"reality_baseline".to_owned()));
        assert!(!tools.contains(&"observe_delta".to_owned()));
        assert!(!tools.contains(&"reality_audit".to_owned()));
        assert!(!tools.contains(&"verification_inbox".to_owned()));
        assert!(!tools.contains(&"verification_poll".to_owned()));
        assert!(!tools.contains(&"verification_audit".to_owned()));
        assert!(!tools.contains(&"verification_bind".to_owned()));
        assert!(!tools.contains(&"verification_sources".to_owned()));
        assert!(
            tools
                .iter()
                .all(|name| PUBLIC_TOOL_NAMES.contains(&name.as_str()))
        );
        assert!(!tools.contains(&"act_click".to_owned()));
        assert!(!tools.contains(&"act_type".to_owned()));
        assert!(!tools.contains(&"release_all".to_owned()));
        assert_debugger_only_hidden(&tools);

        let row = service
            .read_tool_profile_assignment(session_id)
            .expect("read after")
            .expect("row after tools/list profile resolution");
        assert_eq!(row.cf_name, cf::CF_SESSIONS);
        assert_eq!(row.record.profile, ToolProfileKind::NormalAgent);
        assert_eq!(row.record.source, "default_normal_agent");
        assert_eq!(row.record.allowed_tool_count, tools.len());
        assert!(row.value_sha256.starts_with("sha256:"));

        let db = service.m3_storage().expect("storage");
        let stored = db
            .scan_cf_prefix(cf::CF_SESSIONS, &tool_profile_key(session_id))
            .expect("scan policy rows");
        assert_eq!(stored.len(), 1);
        assert_eq!(hex_lower(&stored[0].0), row.key_hex);
        assert_eq!(sha256_hex(&stored[0].1), row.value_sha256);
    }

    #[test]
    fn existing_profile_assignment_self_heals_stale_allowed_surface() {
        let dir = TempDir::new().expect("tmp");
        let service = service_with_db(dir.path());
        let session_id = "issue844-stale-surface-session";
        let fresh = service
            .write_tool_profile_assignment(
                session_id,
                ToolProfileKind::NormalAgent,
                "operator_selected_normal",
                Some("preserve explicit source while refreshing surface".to_owned()),
                Some("operator-session".to_owned()),
            )
            .expect("write fresh row");

        let mut stale = fresh.record.clone();
        stale.allowed_tool_count = 1;
        stale.allowed_tool_sha256 = "sha256:stale-tool-surface".to_owned();
        stale.denied_break_glass_tools = vec!["demo_record_start".to_owned()];
        let stale_encoded = synapse_storage::encode_json(&stale).expect("encode stale row");
        let db = service.m3_storage().expect("storage");
        db.put_batch_pressure_bypass(
            cf::CF_SESSIONS,
            [(tool_profile_key(session_id), stale_encoded.clone())],
        )
        .expect("write stale row");

        let stale_readback = service
            .read_tool_profile_assignment(session_id)
            .expect("read stale row")
            .expect("stale row exists");
        assert_eq!(
            stale_readback.record.allowed_tool_sha256,
            stale.allowed_tool_sha256
        );
        assert_eq!(stale_readback.value_sha256, sha256_hex(&stale_encoded));

        let snapshot = service
            .tool_profile_snapshot(Some(session_id))
            .expect("snapshot self-heals stale surface");
        let row = snapshot.policy_row.as_ref().expect("policy row readback");
        assert_eq!(row.record.profile, ToolProfileKind::NormalAgent);
        assert_eq!(row.record.source, "operator_selected_normal");
        assert_eq!(
            row.record.reason.as_deref(),
            Some("preserve explicit source while refreshing surface")
        );
        assert_eq!(
            row.record.set_by_session_id.as_deref(),
            Some("operator-session")
        );
        assert_eq!(row.record.allowed_tool_count, snapshot.visible_tool_count);
        assert_eq!(row.record.allowed_tool_sha256, snapshot.visible_tool_sha256);
        assert_eq!(
            row.record.denied_break_glass_tools,
            snapshot.denied_break_glass_tools
        );
        assert_ne!(row.value_sha256, sha256_hex(&stale_encoded));
        assert!(snapshot.visible_tool_names.contains(&"profile".to_owned()));
        assert!(
            !snapshot
                .visible_tool_names
                .contains(&"profile_authoring_generate".to_owned())
        );
        assert!(
            !snapshot
                .visible_tool_names
                .contains(&"demo_record_start".to_owned())
        );

        let persisted = service
            .read_tool_profile_assignment(session_id)
            .expect("read healed row")
            .expect("healed row exists");
        assert_eq!(
            persisted.record.allowed_tool_sha256,
            snapshot.visible_tool_sha256
        );
        assert_eq!(persisted.value_sha256, row.value_sha256);
    }

    #[test]
    fn browser_control_profile_persists_public_facade_surface() {
        let dir = TempDir::new().expect("tmp");
        let service = service_with_db(dir.path());
        let session_id = "issue1008-browser-session";
        let row = service
            .write_tool_profile_assignment(
                session_id,
                ToolProfileKind::BrowserControl,
                "test_browser_control",
                Some("dashboard inactive tab verification".to_owned()),
                Some(session_id.to_owned()),
            )
            .expect("write browser profile");
        assert_eq!(row.record.profile, ToolProfileKind::BrowserControl);

        let tools = tool_names(
            service
                .tools_for_session_profile(Some(session_id))
                .expect("browser profile tools"),
        );
        assert_public_facade_surface(&tools);
        assert_eq!(row.record.allowed_tool_count, PUBLIC_TOOL_NAMES.len());
        assert!(tools.contains(&"act".to_owned()));
        assert!(tools.contains(&"target".to_owned()));
        assert!(tools.contains(&"browser_nav".to_owned()));
        assert!(tools.contains(&"browser_debugger".to_owned()));
        assert!(!tools.contains(&"act_run_shell".to_owned()));
        assert!(!tools.contains(&"act_run_shell_start".to_owned()));
        assert!(!tools.contains(&"act_run_shell_status".to_owned()));
        assert!(!tools.contains(&"act_run_shell_cancel".to_owned()));
        assert!(!tools.contains(&"act_spawn_agent".to_owned()));
        assert!(!tools.contains(&"act_type".to_owned()));
        assert_debugger_only_hidden(&tools);
    }

    #[test]
    fn profile_hidden_tool_routes_name_visible_counterpart() {
        let profile_route = hidden_tool_capability_route("profile");
        assert_eq!(profile_route.hidden_tool, "profile");
        assert!(
            profile_route
                .preferred_tools
                .contains(&"tool_profile_status".to_owned())
        );
        assert!(
            profile_route
                .preferred_tools
                .contains(&"tool_profile_set".to_owned())
        );

        let raw_profile_route = hidden_tool_capability_route("tool_profile_set");
        assert_eq!(raw_profile_route.hidden_tool, "tool_profile_set");
        assert!(
            raw_profile_route
                .preferred_tools
                .contains(&"profile operation=status".to_owned())
        );
        assert!(
            raw_profile_route
                .preferred_tools
                .contains(&"profile operation=set".to_owned())
        );
    }

    #[test]
    fn hidden_tool_call_denial_writes_policy_audit_row() {
        let dir = TempDir::new().expect("tmp");
        let service = service_with_db(dir.path());
        let session_id = "issue1008-denied-session";
        let error = service
            .admit_tool_call_for_profile("act_type", Some(session_id))
            .expect_err("normal profile must deny hidden foreground typing tool");
        let code = error
            .data
            .as_ref()
            .and_then(|data| data.get("code"))
            .and_then(Value::as_str);
        assert_eq!(code, Some(error_codes::TOOL_PROFILE_POLICY_DENIED));
        let route = error
            .data
            .as_ref()
            .and_then(|data| data.get("capability_route"))
            .expect("capability route in denial");
        assert_eq!(route["hidden_tool"], "act_type");
        let preferred_tools = route["preferred_tools"]
            .as_array()
            .expect("preferred tools array");
        assert!(
            preferred_tools
                .iter()
                .any(|tool| tool.as_str() == Some("act operation=invoke set_field"))
        );
        assert!(
            preferred_tools
                .iter()
                .any(|tool| tool.as_str() == Some("browser_form operation=set_value"))
        );

        let db = service.m3_storage().expect("storage");
        let audit_rows = db
            .scan_cf_prefix(cf::CF_ACTION_LOG, b"")
            .expect("scan command audit");
        let matching = audit_rows
            .iter()
            .filter(|(_, value)| {
                let text = String::from_utf8_lossy(value);
                text.contains("tool_profile_policy")
                    && text.contains("tool_call_denied")
                    && text.contains("act_type")
                    && text.contains(error_codes::TOOL_PROFILE_POLICY_DENIED)
            })
            .count();
        assert_eq!(matching, 1);
    }

    #[test]
    fn hidden_browser_debugger_tool_denial_names_browser_debugger_route() {
        let dir = TempDir::new().expect("tmp");
        let service = service_with_db(dir.path());
        let session_id = "issue1318-denied-browser-debugger-session";
        let error = service
            .admit_tool_call_for_profile("browser_console_messages", Some(session_id))
            .expect_err("normal profile must deny raw-CDP browser debugger tool");
        let code = error
            .data
            .as_ref()
            .and_then(|data| data.get("code"))
            .and_then(Value::as_str);
        assert_eq!(code, Some(error_codes::TOOL_PROFILE_POLICY_DENIED));
        let route = error
            .data
            .as_ref()
            .and_then(|data| data.get("capability_route"))
            .expect("capability route in denial");
        assert_eq!(route["hidden_tool"], "browser_console_messages");
        let preferred_tools = route["preferred_tools"]
            .as_array()
            .expect("preferred tools array");
        assert!(preferred_tools.iter().any(|tool| {
            tool.as_str()
                .is_some_and(|text| text.contains("profile=browser_debugger"))
        }));
        let resolution = error
            .data
            .as_ref()
            .and_then(|data| data.get("resolution"))
            .and_then(Value::as_str)
            .expect("resolution");
        assert!(resolution.contains("profile=browser_debugger"));

        let db = service.m3_storage().expect("storage");
        let audit_rows = db
            .scan_cf_prefix(cf::CF_ACTION_LOG, b"")
            .expect("scan command audit");
        let matching = audit_rows
            .iter()
            .filter(|(_, value)| {
                let text = String::from_utf8_lossy(value);
                text.contains("tool_profile_policy")
                    && text.contains("tool_call_denied")
                    && text.contains("browser_console_messages")
                    && text.contains("profile=browser_debugger")
                    && text.contains(error_codes::TOOL_PROFILE_POLICY_DENIED)
            })
            .count();
        assert_eq!(matching, 1);
    }

    /// Seeds the cross-session registry with a real `record_initialized` entry
    /// carrying `client_name`, exactly as the HTTP initialize path does
    /// (`transport.rs` -> `record_registry_initialized`).
    fn seed_session_client(service: &SynapseService, session_id: &str, client_name: &str) {
        use rmcp::model::{ClientCapabilities, Implementation, InitializeRequestParams};
        use rmcp::transport::streamable_http_server::session::SessionState;
        let state = SessionState::new(InitializeRequestParams::new(
            ClientCapabilities::default(),
            Implementation::new(client_name, "0.0.0-test"),
        ));
        service
            .session_registry_handle()
            .lock()
            .expect("session registry lock")
            .record_initialized(session_id, &state, "http", 1_000);
    }

    // The facade routes local models need for the same foreground-equivalent
    // capability without exposing raw implementation tools in discovery.
    const LOCAL_AGENT_REQUIRED_FACADE_TOOLS: [&str; 4] =
        ["act", "target", "browser_dom", "browser_debugger"];

    #[test]
    fn local_model_agent_session_gets_full_capability_surface() {
        let dir = TempDir::new().expect("tmp");
        let service = service_with_db(dir.path());
        let session_id = "issue1031-local-session";
        seed_session_client(&service, session_id, "synapse-local-model-agent");

        // tools/list surface SoT: public facades are present and raw input
        // primitives are hidden.
        let tools = tool_names(
            service
                .tools_for_session_profile(Some(session_id))
                .expect("local-agent profile tools"),
        );
        assert_public_facade_surface(&tools);
        for required in LOCAL_AGENT_REQUIRED_FACADE_TOOLS {
            assert!(
                tools.contains(&required.to_owned()),
                "local-model agent must see {required}; visible tool count = {}",
                tools.len()
            );
        }
        assert!(!tools.contains(&"act_type".to_owned()));
        assert!(!tools.contains(&"act_set_field_text".to_owned()));
        assert!(!tools.contains(&"act_click".to_owned()));
        assert!(!tools.contains(&"act_focus_window".to_owned()));

        // Durable policy row SoT: full_capability, auto-assigned source.
        let row = service
            .read_tool_profile_assignment(session_id)
            .expect("read profile row")
            .expect("row exists after resolution");
        assert_eq!(row.record.profile, ToolProfileKind::FullCapability);
        assert_eq!(row.record.source, "default_full_capability_local_agent");
        assert_eq!(
            row.record.denied_break_glass_tools,
            BREAK_GLASS_HAZARDOUS_TOOLS
                .iter()
                .map(|name| (*name).to_owned())
                .collect::<Vec<_>>()
        );

        // Physical CF_SESSIONS readback proves the row is on disk.
        let db = service.m3_storage().expect("storage");
        let stored = db
            .scan_cf_prefix(cf::CF_SESSIONS, &tool_profile_key(session_id))
            .expect("scan policy rows");
        assert_eq!(stored.len(), 1);
        assert_eq!(sha256_hex(&stored[0].1), row.value_sha256);

        // Call-admission gate SoT: facade tools are admitted, raw hidden tools
        // are denied even for full_capability.
        service
            .admit_tool_call_for_profile("act", Some(session_id))
            .expect("full_capability must admit the act facade");
        service
            .admit_tool_call_for_profile("act_type", Some(session_id))
            .expect_err("full_capability must not expose raw act_type directly");
    }

    #[test]
    fn non_local_agent_session_stays_least_privilege() {
        let dir = TempDir::new().expect("tmp");
        let service = service_with_db(dir.path());
        let session_id = "issue1031-codex-session";
        seed_session_client(&service, session_id, "codex-cli");

        let tools = tool_names(
            service
                .tools_for_session_profile(Some(session_id))
                .expect("codex profile tools"),
        );
        assert!(tools.contains(&"health".to_owned()));
        for hidden in [
            "act_type",
            "act_set_field_text",
            "act_click",
            "act_focus_window",
        ] {
            assert!(
                !tools.contains(&hidden.to_owned()),
                "non-local agent must NOT see foreground primitive {hidden}"
            );
        }
        let row = service
            .read_tool_profile_assignment(session_id)
            .expect("read profile row")
            .expect("row exists after resolution");
        assert_eq!(row.record.profile, ToolProfileKind::NormalAgent);
        assert_eq!(row.record.source, "default_normal_agent");
        service
            .admit_tool_call_for_profile("act_type", Some(session_id))
            .expect_err("normal_agent must deny act_type");
    }

    #[test]
    fn local_model_session_self_heals_stale_normal_default() {
        let dir = TempDir::new().expect("tmp");
        let service = service_with_db(dir.path());
        let session_id = "issue1031-selfheal-session";

        // Simulate a least-privilege row written before the MCP client identity
        // was known to the registry.
        service
            .write_tool_profile_assignment(
                session_id,
                ToolProfileKind::NormalAgent,
                "default_normal_agent",
                None,
                None,
            )
            .expect("seed stale normal-agent row");
        let before = service
            .read_tool_profile_assignment(session_id)
            .expect("read before")
            .expect("row before");
        assert_eq!(before.record.profile, ToolProfileKind::NormalAgent);

        // Identity now classifies the session as the local-model harness.
        seed_session_client(&service, session_id, "synapse-local-model-agent");

        // Resolving the surface upgrades the durable row in place.
        let tools = tool_names(
            service
                .tools_for_session_profile(Some(session_id))
                .expect("post-heal tools"),
        );
        assert_public_facade_surface(&tools);
        assert!(tools.contains(&"act".to_owned()));
        assert!(!tools.contains(&"act_type".to_owned()));
        let after = service
            .read_tool_profile_assignment(session_id)
            .expect("read after")
            .expect("row after");
        assert_eq!(after.record.profile, ToolProfileKind::FullCapability);
        assert_eq!(after.record.source, "default_full_capability_local_agent");
    }

    #[test]
    fn explicit_tool_profile_set_to_full_capability_requires_lease_proof() {
        // The frictionless path to full_capability is the auto default; an
        // explicit escalation by hand must carry the same proof as break_glass
        // so no agent can self-escalate to the foreground primitives.
        let proof = ToolProfileLeaseProof {
            required: true,
            held: false,
            owner_session_id: None,
            caller_is_owner: false,
            expires_in_ms: None,
        };
        assert!(
            validate_profile_set_policy(
                "s1",
                ToolProfileKind::FullCapability,
                Some("need full surface"),
                true,
                &proof,
            )
            .is_err()
        );
    }

    /// Measurement probe (not a regression gate): emit the EXACT public facade
    /// tool surface a Synapse-spawned local-model agent (gemma/DeepSeek)
    /// receives under FullCapability policy, as physical source of truth, plus
    /// the real byte size + token estimate of the OpenAI `tools` array that the
    /// local-agent harness puts on the chat-completion request body.
    ///
    /// Faithful reproduction of the production path:
    ///   1. a session whose MCP client identity is the local-model harness
    ///      (`synapse-local-model-agent`) resolves to `ToolProfileKind::FullCapability`,
    ///   2. `tools_for_session_profile` returns the <=40 public facade tools
    ///      sorted exactly as the handler emits them — the same `Vec<Tool>` the
    ///      agent's `tools/list` receives,
    ///   3. each `Tool` is mapped through the SAME JSON shape as
    ///      `local_agent::openai_tool_from_mcp` (kept in sync below) to build the
    ///      `tools[]` field of the request body.
    ///
    /// Skipped unless `SYNAPSE_TOOL_SURFACE_OUT` is set to the absolute output
    /// path, so it never writes during a normal `cargo test` run.
    ///
    /// ```text
    /// SYNAPSE_TOOL_SURFACE_OUT=C:/code/synapse-subconscious/artifacts/tool_surface.json \
    ///   cargo test -p synapse-mcp --lib -- --ignored --exact \
    ///   server::tool_profiles::tests::emit_full_capability_tool_surface_artifact --nocapture
    /// ```
    #[test]
    #[ignore = "measurement probe; set SYNAPSE_TOOL_SURFACE_OUT to run"]
    fn emit_full_capability_tool_surface_artifact() {
        let Ok(out_path) = std::env::var("SYNAPSE_TOOL_SURFACE_OUT") else {
            eprintln!("SYNAPSE_TOOL_SURFACE_OUT not set; skipping artifact emission");
            return;
        };

        let dir = TempDir::new().expect("tmp");
        let service = service_with_db(dir.path());
        let session_id = "tool-surface-probe-local-session";
        // Production identity for the Synapse local-model harness; this is what
        // makes the session resolve to FullCapability automatically.
        seed_session_client(&service, session_id, "synapse-local-model-agent");

        // The exact Vec<Tool> the agent's tools/list receives for this session.
        let tools = service
            .tools_for_session_profile(Some(session_id))
            .expect("full-capability facade tool surface");

        // Hard SoT assertion: this session really is FullCapability.
        let row = service
            .read_tool_profile_assignment(session_id)
            .expect("read profile row")
            .expect("row exists");
        assert_eq!(row.record.profile, ToolProfileKind::FullCapability);

        // Map each Tool exactly as local_agent::openai_tool_from_mcp does.
        // (That fn lives in the binary's local_agent module; its body is a pure
        // JSON wrapper with no schema logic — the schema work already happened in
        // sanitize_tools above. Kept byte-identical here.)
        let openai_tool_from_mcp = |tool: &Tool| -> serde_json::Value {
            serde_json::json!({
                "type": "function",
                "function": {
                    "name": tool.name.as_ref(),
                    "description": tool
                        .description
                        .as_ref()
                        .map(|desc| desc.as_ref())
                        .unwrap_or("Synapse MCP tool"),
                    "parameters": serde_json::Value::Object((*tool.input_schema).clone()),
                }
            })
        };

        let openai_tools: Vec<serde_json::Value> = tools.iter().map(openai_tool_from_mcp).collect();

        // The actual `tools` field of the chat-completion request body.
        let openai_tools_json =
            serde_json::to_string(&openai_tools).expect("serialize openai tools array");
        let openai_tools_bytes = openai_tools_json.len();
        let openai_tools_chars = openai_tools_json.chars().count();

        // Per-tool detail (sanitized schema, exactly as emitted to the model) and
        // the longest descriptions by byte size.
        let mut tool_entries: Vec<serde_json::Value> = Vec::with_capacity(tools.len());
        let mut desc_sizes: Vec<(String, usize)> = Vec::with_capacity(tools.len());
        for tool in &tools {
            let name = tool.name.to_string();
            let description = tool
                .description
                .as_ref()
                .map(|desc| desc.as_ref())
                .unwrap_or("Synapse MCP tool")
                .to_owned();
            desc_sizes.push((name.clone(), description.len()));
            tool_entries.push(serde_json::json!({
                "name": name,
                "description": description,
                "input_schema": serde_json::Value::Object((*tool.input_schema).clone()),
            }));
        }
        desc_sizes.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        let longest_descriptions: Vec<serde_json::Value> = desc_sizes
            .iter()
            .take(10)
            .map(|(name, bytes)| serde_json::json!({ "name": name, "description_bytes": bytes }))
            .collect();

        // Token estimates over the actual openai_tools payload. No real
        // tokenizer crate is in the workspace, so report both common heuristics,
        // clearly labeled as estimates.
        let approx_tokens_chars_div_4 = (openai_tools_chars as f64 / 4.0).ceil() as u64;
        let approx_tokens_chars_div_3_5 = (openai_tools_chars as f64 / 3.5).ceil() as u64;

        let artifact = serde_json::json!({
            "_meta": {
                "description": "Exact <=40 public facade MCP tool surface a Synapse-spawned local-model agent (e.g. gemma4) receives under FullCapability policy, with the real openai tools[] payload size and token estimate.",
                "profile": row.record.profile.as_str(),
                "profile_source": row.record.source,
                "client_identity": "synapse-local-model-agent",
                "produced_by": "server::tool_profiles::tests::emit_full_capability_tool_surface_artifact",
                "env_gates": {
                    "SYNAPSE_DEBUG_TOOLS": std::env::var("SYNAPSE_DEBUG_TOOLS").ok(),
                },
            },
            "tool_count": tools.len(),
            "openai_tools_bytes": openai_tools_bytes,
            "openai_tools_chars": openai_tools_chars,
            "approx_tokens": {
                "note": "No real tokenizer crate is vendored in this workspace; both values are heuristic estimates over the openai_tools payload.",
                "chars_div_4": approx_tokens_chars_div_4,
                "chars_div_3_5": approx_tokens_chars_div_3_5,
            },
            "longest_descriptions_by_bytes": longest_descriptions,
            "tools": tool_entries,
        });

        let out = std::path::Path::new(&out_path);
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent).expect("create artifact parent dir");
        }
        let serialized = serde_json::to_string_pretty(&artifact).expect("serialize artifact");
        std::fs::write(out, serialized).expect("write artifact");

        eprintln!("TOOL_SURFACE tool_count={}", tools.len());
        eprintln!("TOOL_SURFACE openai_tools_bytes={openai_tools_bytes}");
        eprintln!("TOOL_SURFACE openai_tools_chars={openai_tools_chars}");
        eprintln!("TOOL_SURFACE approx_tokens_chars_div_4={approx_tokens_chars_div_4}");
        eprintln!("TOOL_SURFACE approx_tokens_chars_div_3_5={approx_tokens_chars_div_3_5}");
        for (name, bytes) in desc_sizes.iter().take(10) {
            eprintln!("TOOL_SURFACE longest_desc {name} {bytes}");
        }
        eprintln!("TOOL_SURFACE written_to={out_path}");
    }
}
