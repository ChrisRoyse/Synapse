// === Perception (06 section 8.1) ===
pub const OBSERVE_NO_PERCEPTION_AVAILABLE: &str = "OBSERVE_NO_PERCEPTION_AVAILABLE";
pub const OBSERVE_INTERNAL: &str = "OBSERVE_INTERNAL";
pub const CAPTURE_GRAPHICS_API_UNSUPPORTED: &str = "CAPTURE_GRAPHICS_API_UNSUPPORTED";
pub const CAPTURE_PRINTWINDOW_DISABLED: &str = "CAPTURE_PRINTWINDOW_DISABLED";
pub const CAPTURE_PRINTWINDOW_BLACK: &str = "CAPTURE_PRINTWINDOW_BLACK";
pub const CAPTURE_TARGET_LOST: &str = "CAPTURE_TARGET_LOST";
pub const CAPTURE_NO_DIRTY_REGIONS: &str = "CAPTURE_NO_DIRTY_REGIONS";
/// A screenshot request's independently calculated capture/composition/message
/// plan cannot fit the hard pipeline budget. This is rejected before any page
/// mutation or Chrome capture begins (#2170).
pub const CAPTURE_PLAN_EXCEEDS_LIMIT: &str = "CAPTURE_PLAN_EXCEEDS_LIMIT";
pub const A11Y_NOT_AVAILABLE: &str = "A11Y_NOT_AVAILABLE";
pub const A11Y_ELEMENT_STALE: &str = "A11Y_ELEMENT_STALE";
pub const A11Y_NO_FOREGROUND: &str = "A11Y_NO_FOREGROUND";
pub const A11Y_CDP_UNREACHABLE: &str = "A11Y_CDP_UNREACHABLE";
pub const A11Y_CDP_ATTACH_FAILED: &str = "A11Y_CDP_ATTACH_FAILED";
pub const A11Y_CDP_AXTREE_FAILED: &str = "A11Y_CDP_AXTREE_FAILED";
pub const A11Y_CDP_EXTENSION_UNAVAILABLE: &str = "A11Y_CDP_EXTENSION_UNAVAILABLE";
pub const A11Y_CDP_EXTENSION_DETACHED: &str = "A11Y_CDP_EXTENSION_DETACHED";
pub const A11Y_CDP_EXTENSION_TIMEOUT: &str = "A11Y_CDP_EXTENSION_TIMEOUT";
pub const A11Y_CDP_DEBUGGER_WARNING_UNSUPPRESSED: &str = "A11Y_CDP_DEBUGGER_WARNING_UNSUPPRESSED";
pub const CHROME_BRIDGE_EXTENSION_STALE: &str = "CHROME_BRIDGE_EXTENSION_STALE";
/// The authenticated Chrome extension returned a missing or unregistered
/// machine error identifier.
///
/// The original code/detail are retained in the diagnostic, but never
/// reclassified as an unrelated browser failure.
pub const CHROME_BRIDGE_ERROR_CODE_CONTRACT_VIOLATION: &str =
    "CHROME_BRIDGE_ERROR_CODE_CONTRACT_VIOLATION";
/// The authenticated Chrome bridge refused a command response because its
/// exact serialized HTTP envelope exceeded the shared bounded transport limit.
pub const CHROME_BRIDGE_MESSAGE_BODY_EXCEEDS_LIMIT: &str =
    "CHROME_BRIDGE_MESSAGE_BODY_EXCEEDS_LIMIT";
/// The extension executed a Chrome command, but the exact serialized result
/// could not fit the bounded, authenticated terminal channel.
///
/// The command is never replayed; callers must request a smaller result.
pub const A11Y_CDP_RESPONSE_TOO_LARGE: &str = "A11Y_CDP_RESPONSE_TOO_LARGE";
/// A command-terminal envelope, acknowledgement, replay, digest, sequence, or
/// ownership claim contradicted the daemon's ledger. The bridge fails closed.
pub const CHROME_BRIDGE_TERMINAL_PROTOCOL_ERROR: &str = "CHROME_BRIDGE_TERMINAL_PROTOCOL_ERROR";
pub const CHROME_CAPTURE_VISIBLE_TAB_PENDING: &str = "CHROME_CAPTURE_VISIBLE_TAB_PENDING";
/// The host-side exact Chrome extension management control could not reload or
/// install the normal-profile bridge, or its independent profile/host readback
/// did not prove the requested transition.
pub const CHROME_BRIDGE_HOST_RELOAD_FAILED: &str = "CHROME_BRIDGE_HOST_RELOAD_FAILED";
pub const CHROME_SCRIPTING_EXECUTE_FAILED: &str = "CHROME_SCRIPTING_EXECUTE_FAILED";
pub const CHROME_SCRIPTING_EMPTY_RESULT: &str = "CHROME_SCRIPTING_EMPTY_RESULT";
pub const CHROME_SCRIPTING_UNAVAILABLE: &str = "CHROME_SCRIPTING_UNAVAILABLE";
pub const BROWSER_URL_SCHEME_UNSUPPORTED: &str = "BROWSER_URL_SCHEME_UNSUPPORTED";
pub const CHROME_DOM_SELECTOR_INVALID: &str = "CHROME_DOM_SELECTOR_INVALID";
pub const CHROME_DOM_ELEMENT_NOT_FOUND: &str = "CHROME_DOM_ELEMENT_NOT_FOUND";
pub const CHROME_DOM_ELEMENT_AMBIGUOUS: &str = "CHROME_DOM_ELEMENT_AMBIGUOUS";
pub const CHROME_DOM_ELEMENT_NOT_ACTIONABLE: &str = "CHROME_DOM_ELEMENT_NOT_ACTIONABLE";
pub const CHROME_DOM_ACTION_UNSUPPORTED: &str = "CHROME_DOM_ACTION_UNSUPPORTED";
pub const CHROME_DOM_ACTION_POSTCONDITION_FAILED: &str = "CHROME_DOM_ACTION_POSTCONDITION_FAILED";
pub const BROWSER_WAIT_TIMEOUT: &str = "BROWSER_WAIT_TIMEOUT";
/// Emitted when an evaluate expression outlives its `timeout_ms` budget.
///
/// Distinct from `BROWSER_EVALUATE_JAVASCRIPT_EXCEPTION`, which means CDP
/// completed the transport operation and the evaluated page program threw.
/// The message carries the elapsed and budget milliseconds so an agent can
/// retry with a larger `timeout_ms` rather than guessing.
pub const BROWSER_EVALUATE_TIMEOUT: &str = "BROWSER_EVALUATE_TIMEOUT";
/// `Runtime.evaluate` completed in Chrome, but returned
/// `exceptionDetails` for the caller's JavaScript.
///
/// This is a page-program failure, not a debugger attach/transport failure.
pub const BROWSER_EVALUATE_JAVASCRIPT_EXCEPTION: &str = "BROWSER_EVALUATE_JAVASCRIPT_EXCEPTION";
pub const BROWSER_NAVIGATION_FAILED: &str = "BROWSER_NAVIGATION_FAILED";
pub const CHROME_ACTIVE_ELEMENT_MISSING: &str = "CHROME_ACTIVE_ELEMENT_MISSING";
pub const CHROME_ACTIVE_ELEMENT_NOT_EDITABLE: &str = "CHROME_ACTIVE_ELEMENT_NOT_EDITABLE";
pub const CHROME_ACTIVE_ELEMENT_VALUE_MISMATCH: &str = "CHROME_ACTIVE_ELEMENT_VALUE_MISMATCH";
pub const CHROME_BEFOREINPUT_CANCELLED: &str = "CHROME_BEFOREINPUT_CANCELLED";
pub const CHROME_CLOCK_FAILED: &str = "CHROME_CLOCK_FAILED";
pub const CHROME_FRAME_METADATA_FAILED: &str = "CHROME_FRAME_METADATA_FAILED";
pub const CHROME_SET_FIELD_BAD_LOCATOR: &str = "CHROME_SET_FIELD_BAD_LOCATOR";
pub const CHROME_SET_FIELD_NOT_FOUND: &str = "CHROME_SET_FIELD_NOT_FOUND";
pub const CHROME_SET_FIELD_NOT_UNIQUE: &str = "CHROME_SET_FIELD_NOT_UNIQUE";
pub const CHROME_SET_FIELD_SELECTOR_INVALID: &str = "CHROME_SET_FIELD_SELECTOR_INVALID";
pub const CHROME_SET_FIELD_VALUE_MISMATCH: &str = "CHROME_SET_FIELD_VALUE_MISMATCH";
pub const CHROME_STORAGE_ACTION_FAILED: &str = "CHROME_STORAGE_ACTION_FAILED";
pub const CHROME_STORAGE_KEY_INVALID: &str = "CHROME_STORAGE_KEY_INVALID";
pub const CHROME_STORAGE_OPERATION_UNSUPPORTED: &str = "CHROME_STORAGE_OPERATION_UNSUPPORTED";
pub const CHROME_STORAGE_STATE_LOAD_FAILED: &str = "CHROME_STORAGE_STATE_LOAD_FAILED";
pub const CHROME_STORAGE_STATE_READ_FAILED: &str = "CHROME_STORAGE_STATE_READ_FAILED";
/// A caller-supplied browser wait predicate could not be compiled or threw in
/// the exact target document. Distinct from debugger attach/transport failure.
pub const CHROME_WAIT_PREDICATE_INVALID: &str = "CHROME_WAIT_PREDICATE_INVALID";
pub const PAGE_VITALS_READ_FAILED: &str = "PAGE_VITALS_READ_FAILED";
pub const SYNAPSE_CHROME_BRIDGE_MAINTENANCE_PAUSE_PERSIST_FAILED: &str =
    "SYNAPSE_CHROME_BRIDGE_MAINTENANCE_PAUSE_PERSIST_FAILED";
pub const SYNAPSE_CHROME_BRIDGE_RECONNECT_WAKE_ALARM_INVALID: &str =
    "SYNAPSE_CHROME_BRIDGE_RECONNECT_WAKE_ALARM_INVALID";
pub const SYNAPSE_CHROME_DAEMON_UNAVAILABLE: &str = "SYNAPSE_CHROME_DAEMON_UNAVAILABLE";
pub const SYNAPSE_CHROME_EXTENSION_ID_MISMATCH: &str = "SYNAPSE_CHROME_EXTENSION_ID_MISMATCH";
pub const A11Y_UIA_WORKER_TIMEOUT: &str = "A11Y_UIA_WORKER_TIMEOUT";
pub const A11Y_TARGET_WINDOW_MINIMIZED_UIA_UNAVAILABLE: &str =
    "A11Y_TARGET_WINDOW_MINIMIZED_UIA_UNAVAILABLE";
pub const DETECTION_MODEL_NOT_LOADED: &str = "DETECTION_MODEL_NOT_LOADED";
pub const DETECTION_MODEL_INFER_FAILED: &str = "DETECTION_MODEL_INFER_FAILED";
pub const DETECTION_NO_FRAME: &str = "DETECTION_NO_FRAME";
/// The active profile asks for no detector inference at all (#2054).
///
/// Not a tool error: detection is profile-opt-in, so a profile with no
/// `[detection]` model is a legitimate configuration. It is the `reason_code`
/// carried by `SensorStatus::NotConfigured` on `diagnostics.detection_status`,
/// and the `reason_code` of `health.subsystems.perception.perception_detection`,
/// so neither surface can report a detector that never ran as `healthy`/`ok`.
pub const DETECTION_NOT_CONFIGURED: &str = "DETECTION_NOT_CONFIGURED";
pub const OCR_NO_TEXT: &str = "OCR_NO_TEXT";
pub const OCR_BACKEND_UNAVAILABLE: &str = "OCR_BACKEND_UNAVAILABLE";
/// Bound-tab OCR refused because the window renders a different tab (#1823).
///
/// The MCP session is bound to a specific browser tab, but window capture (WGC)
/// can only observe whichever tab is currently rendered in that window.
/// Returning OCR of the rendered tab under a different tab's binding is
/// confidently-wrong perception, so the read fails closed with this code naming
/// both the bound tab and the tab actually rendered.
pub const OCR_TARGET_NOT_FOREGROUND: &str = "OCR_TARGET_NOT_FOREGROUND";
// Per-agent active target (epic #720): each MCP session can bind its own window/CDP
// target so observe/find/read_text perceive it without stealing the global foreground.
pub const TARGET_WINDOW_NOT_FOUND: &str = "TARGET_WINDOW_NOT_FOUND";
pub const TARGET_NOT_SET: &str = "TARGET_NOT_SET";
pub const TARGET_CDP_UNRESOLVED: &str = "TARGET_CDP_UNRESOLVED";
pub const TARGET_CO_OWNED: &str = "TARGET_CO_OWNED";
pub const TARGET_CLAIM_NOT_FOUND: &str = "TARGET_CLAIM_NOT_FOUND";
pub const TARGET_CLAIM_ADOPT_REFUSED: &str = "TARGET_CLAIM_ADOPT_REFUSED";
pub const TARGET_CLAIM_OWNER_ACTIVE: &str = "TARGET_CLAIM_OWNER_ACTIVE";
pub const HUD_NO_ACTIVE_PROFILE: &str = "HUD_NO_ACTIVE_PROFILE";
pub const HUD_FIELD_NOT_DEFINED: &str = "HUD_FIELD_NOT_DEFINED";
pub const HUD_EXTRACTION_FAILED: &str = "HUD_EXTRACTION_FAILED";
pub const AUDIO_DEVICE_LOST: &str = "AUDIO_DEVICE_LOST";
pub const AUDIO_LOOPBACK_INIT_FAILED: &str = "AUDIO_LOOPBACK_INIT_FAILED";
pub const AUDIO_TIMELINE_DISCONTINUITY: &str = "AUDIO_TIMELINE_DISCONTINUITY";
pub const AUDIO_TIMELINE_GAP: &str = "AUDIO_TIMELINE_GAP";
pub const AUDIO_TIMELINE_INVALID: &str = "AUDIO_TIMELINE_INVALID";
pub const AUDIO_STT_MODEL_NOT_LOADED: &str = "AUDIO_STT_MODEL_NOT_LOADED";

// === Action (06 section 8.2) ===
pub const ACTION_QUEUE_FULL: &str = "ACTION_QUEUE_FULL";
pub const ACTION_RATE_LIMITED: &str = "ACTION_RATE_LIMITED";
pub const ACTION_BACKEND_UNAVAILABLE: &str = "ACTION_BACKEND_UNAVAILABLE";
pub const ACTION_TARGET_INVALID: &str = "ACTION_TARGET_INVALID";
pub const ACTION_HOLD_EXCEEDED_MAX: &str = "ACTION_HOLD_EXCEEDED_MAX";
pub const ACTION_VIGEM_NOT_INSTALLED: &str = "ACTION_VIGEM_NOT_INSTALLED";
pub const ACTION_VIGEM_PLUGIN_FAILED: &str = "ACTION_VIGEM_PLUGIN_FAILED";
pub const ACTION_ELEMENT_NOT_RESOLVED: &str = "ACTION_ELEMENT_NOT_RESOLVED";
pub const ACTION_ELEMENT_PATTERN_UNSUPPORTED: &str = "ACTION_ELEMENT_PATTERN_UNSUPPORTED";
pub const TRANSIENT_ELEMENT_EXPIRED: &str = "TRANSIENT_ELEMENT_EXPIRED";
pub const ACTION_FOREGROUND_LOST: &str = "ACTION_FOREGROUND_LOST";
pub const ACTION_NO_OBSERVED_DELTA: &str = "ACTION_NO_OBSERVED_DELTA";
pub const ACTION_VERIFY_SURFACE_UNAVAILABLE: &str = "ACTION_VERIFY_SURFACE_UNAVAILABLE";
pub const ACTION_POSTCONDITION_FAILED: &str = "ACTION_POSTCONDITION_FAILED";
pub const ACTION_LAUNCH_WINDOW_NOT_FOUND: &str = "ACTION_LAUNCH_WINDOW_NOT_FOUND";
pub const ACTION_LAUNCH_FOREGROUND_FAILED: &str = "ACTION_LAUNCH_FOREGROUND_FAILED";
pub const ACTION_LAUNCH_URL_NOT_REACHED: &str = "ACTION_LAUNCH_URL_NOT_REACHED";
pub const ACTION_AGENT_SPAWN_FAILED: &str = "ACTION_AGENT_SPAWN_FAILED";
pub const ACTION_AGENT_SPAWN_SESSION_TIMEOUT: &str = "ACTION_AGENT_SPAWN_SESSION_TIMEOUT";
pub const ACTION_AGENT_SPAWN_TASK_NOT_STARTED: &str = "ACTION_AGENT_SPAWN_TASK_NOT_STARTED";
pub const ACTION_BUDGET_EXPIRED: &str = "ACTION_BUDGET_EXPIRED";
pub const ACTION_WINDOW_NOT_FOUND: &str = "ACTION_WINDOW_NOT_FOUND";
pub const ACTION_WINDOW_AMBIGUOUS: &str = "ACTION_WINDOW_AMBIGUOUS";
pub const ACTION_FOCUS_WINDOW_FAILED: &str = "ACTION_FOCUS_WINDOW_FAILED";
pub const ACTION_UNSUPPORTED_KEY: &str = "ACTION_UNSUPPORTED_KEY";
pub const ACTION_DRAG_DISTANCE_EXCEEDS_LIMIT: &str = "ACTION_DRAG_DISTANCE_EXCEEDS_LIMIT";
pub const STUCK_KEY_AUTO_RELEASED: &str = "STUCK_KEY_AUTO_RELEASED";
pub const SAFETY_RELEASE_ALL_FIRED: &str = "SAFETY_RELEASE_ALL_FIRED";
pub const SAFETY_OPERATOR_HOTKEY_FIRED: &str = "SAFETY_OPERATOR_HOTKEY_FIRED";
// Multi-agent input lease (epic #719): the real foreground/cursor/keyboard/clipboard
// is a single shared resource leased per MCP session. Background tiers never take it.
pub const ACTION_FOREGROUND_LEASE_BUSY: &str = "ACTION_FOREGROUND_LEASE_BUSY";
pub const ACTION_FOREGROUND_LEASE_NOT_HELD: &str = "ACTION_FOREGROUND_LEASE_NOT_HELD";
pub const FOREGROUND_ACTIVATION_REFUSED: &str = "FOREGROUND_ACTIVATION_REFUSED";
pub const ACTION_FOREGROUND_CONTEXT_CAPTURE_FAILED: &str =
    "ACTION_FOREGROUND_CONTEXT_CAPTURE_FAILED";
pub const ACTION_FOREGROUND_CONTEXT_RESTORE_FAILED: &str =
    "ACTION_FOREGROUND_CONTEXT_RESTORE_FAILED";
pub const ACTION_FOREGROUND_CONTEXT_RESTORE_SKIPPED: &str =
    "ACTION_FOREGROUND_CONTEXT_RESTORE_SKIPPED";
pub const FOREGROUND_RESTORE_SKIPPED_HUMAN_MOVED: &str = "FOREGROUND_RESTORE_SKIPPED_HUMAN_MOVED";
pub const ACTION_ELEMENT_VALUE_READ_ONLY: &str = "ACTION_ELEMENT_VALUE_READ_ONLY";
pub const ACTION_REMOTE_PROCESS_CLEANUP_UNVERIFIED: &str =
    "ACTION_REMOTE_PROCESS_CLEANUP_UNVERIFIED";

// === Reflex (06 section 8.3) ===
pub const REFLEX_CAP_REACHED: &str = "REFLEX_CAP_REACHED";
pub const REFLEX_KIND_INVALID: &str = "REFLEX_KIND_INVALID";
pub const REFLEX_PARAMS_INVALID: &str = "REFLEX_PARAMS_INVALID";
pub const REFLEX_TARGET_INVALID: &str = "REFLEX_TARGET_INVALID";
pub const REFLEX_FILTER_INVALID: &str = "REFLEX_FILTER_INVALID";
pub const REFLEX_PRIORITY_INVALID: &str = "REFLEX_PRIORITY_INVALID";
pub const REFLEX_AUDIT_TIMESTAMP_INVALID: &str = "REFLEX_AUDIT_TIMESTAMP_INVALID";
pub const REFLEX_TICK_LATE: &str = "REFLEX_TICK_LATE";
pub const REFLEX_TRACK_LOST: &str = "REFLEX_TRACK_LOST";
pub const REFLEX_STARVED: &str = "REFLEX_STARVED";
pub const REFLEX_DISABLED_BY_OPERATOR: &str = "REFLEX_DISABLED_BY_OPERATOR";
pub const REFLEX_DURABLE_DEFINITION_MISSING: &str = "REFLEX_DURABLE_DEFINITION_MISSING";
pub const REFLEX_LIFETIME_EXPIRED: &str = "REFLEX_LIFETIME_EXPIRED";
pub const REFLEX_RECURSION_LIMIT: &str = "REFLEX_RECURSION_LIMIT";
pub const REFLEX_ACTION_PERMISSION_DENIED: &str = "REFLEX_ACTION_PERMISSION_DENIED";
pub const REFLEX_DEBOUNCED: &str = "REFLEX_DEBOUNCED";

// === Profile and config (06 section 8.4) ===
pub const PROFILE_NOT_FOUND: &str = "PROFILE_NOT_FOUND";
pub const PROFILE_PARSE_ERROR: &str = "PROFILE_PARSE_ERROR";
pub const PROFILE_VERSION_INCOMPATIBLE: &str = "PROFILE_VERSION_INCOMPATIBLE";
pub const PROFILE_KEYMAP_INVALID: &str = "PROFILE_KEYMAP_INVALID";
pub const PROFILE_HUD_REGION_INVALID: &str = "PROFILE_HUD_REGION_INVALID";
pub const PROFILE_TRUST_VERIFICATION_FAILED: &str = "PROFILE_TRUST_VERIFICATION_FAILED";
pub const PROFILE_ROLLBACK_UNAVAILABLE: &str = "PROFILE_ROLLBACK_UNAVAILABLE";
pub const AUDIT_EXPORT_CONSENT_REQUIRED: &str = "AUDIT_EXPORT_CONSENT_REQUIRED";
pub const AUDIT_EXPORT_REDACTION_REQUIRED: &str = "AUDIT_EXPORT_REDACTION_REQUIRED";
pub const AUDIT_EXPORT_PAYLOAD_TOO_LARGE: &str = "AUDIT_EXPORT_PAYLOAD_TOO_LARGE";
pub const PROFILE_AUTHORING_INSUFFICIENT_EVIDENCE: &str = "PROFILE_AUTHORING_INSUFFICIENT_EVIDENCE";
pub const PROFILE_AUTHORING_CONFLICTING_EVIDENCE: &str = "PROFILE_AUTHORING_CONFLICTING_EVIDENCE";
pub const PROFILE_AUTHORING_UNSAFE_ESCALATION: &str = "PROFILE_AUTHORING_UNSAFE_ESCALATION";
pub const PROFILE_AUTHORING_CANDIDATE_NOT_FOUND: &str = "PROFILE_AUTHORING_CANDIDATE_NOT_FOUND";
pub const PROFILE_AUTHORING_INVALID_STATE: &str = "PROFILE_AUTHORING_INVALID_STATE";
pub const CAPTURE_TARGET_INVALID: &str = "CAPTURE_TARGET_INVALID";
pub const PERCEPTION_MODE_INVALID: &str = "PERCEPTION_MODE_INVALID";

// === MCP and session (06 section 8.5) ===
pub const SESSION_NOT_FOUND: &str = "SESSION_NOT_FOUND";
pub const SESSION_EXPIRED: &str = "SESSION_EXPIRED";
pub const RECIPIENT_UNKNOWN: &str = "RECIPIENT_UNKNOWN";
pub const SUBSCRIPTION_NOT_FOUND: &str = "SUBSCRIPTION_NOT_FOUND";
pub const SUBSCRIPTION_CAP_REACHED: &str = "SUBSCRIPTION_CAP_REACHED";
pub const TOOL_NOT_FOUND: &str = "TOOL_NOT_FOUND";
pub const TOOL_PROFILE_POLICY_DENIED: &str = "TOOL_PROFILE_POLICY_DENIED";
pub const TOOL_PARAMS_INVALID: &str = "TOOL_PARAMS_INVALID";
/// Autonomous routine arming was refused because grounded eligibility failed.
pub const ROUTINE_AUTONOMY_NOT_READY: &str = "ROUTINE_AUTONOMY_NOT_READY";
pub const TOOL_INTERNAL_ERROR: &str = "TOOL_INTERNAL_ERROR";
pub const HTTP_BIND_ADDRESS_INVALID: &str = "HTTP_BIND_ADDRESS_INVALID";
pub const HTTP_BIND_NON_LOOPBACK_REFUSED: &str = "HTTP_BIND_NON_LOOPBACK_REFUSED";
pub const HTTP_TOKEN_INVALID: &str = "HTTP_TOKEN_INVALID";
pub const HTTP_ORIGIN_REFUSED: &str = "HTTP_ORIGIN_REFUSED";
pub const HTTP_SESSION_INVALID: &str = "HTTP_SESSION_INVALID";
pub const DAEMON_RESTARTING: &str = "DAEMON_RESTARTING";
pub const REPLAY_TARGET_INVALID: &str = "REPLAY_TARGET_INVALID";
pub const REPLAY_FORMAT_INVALID: &str = "REPLAY_FORMAT_INVALID";

// === Storage (06 section 8.6) ===
pub const STORAGE_OPEN_FAILED: &str = "STORAGE_OPEN_FAILED";
pub const STORAGE_BACKEND_INVALID_CONFIG: &str = "STORAGE_BACKEND_INVALID_CONFIG";
pub const STORAGE_BACKEND_UNIMPLEMENTED: &str = "STORAGE_BACKEND_UNIMPLEMENTED";
pub const STORAGE_WRITE_FAILED: &str = "STORAGE_WRITE_FAILED";
pub const STORAGE_READ_FAILED: &str = "STORAGE_READ_FAILED";
pub const STORAGE_CORRUPTED: &str = "STORAGE_CORRUPTED";
pub const STORAGE_SCHEMA_MISMATCH: &str = "STORAGE_SCHEMA_MISMATCH";
pub const STORAGE_DISK_PRESSURE_LEVEL_1: &str = "STORAGE_DISK_PRESSURE_LEVEL_1";
pub const STORAGE_DISK_PRESSURE_LEVEL_2: &str = "STORAGE_DISK_PRESSURE_LEVEL_2";
pub const STORAGE_DISK_PRESSURE_LEVEL_3: &str = "STORAGE_DISK_PRESSURE_LEVEL_3";
pub const STORAGE_DISK_PRESSURE_LEVEL_4: &str = "STORAGE_DISK_PRESSURE_LEVEL_4";
pub const STORAGE_CF_HARD_CAP_REACHED: &str = "STORAGE_CF_HARD_CAP_REACHED";
pub const STORAGE_GC_UNSAFE_EVICTION_REFUSED: &str = "STORAGE_GC_UNSAFE_EVICTION_REFUSED";
pub const STORAGE_SEARCH_REBUILD_IN_PROGRESS: &str = "STORAGE_SEARCH_REBUILD_IN_PROGRESS";
pub const STORAGE_PANEL_LIFECYCLE_IN_PROGRESS: &str = "STORAGE_PANEL_LIFECYCLE_IN_PROGRESS";
pub const STORAGE_BACKUP_IN_PROGRESS: &str = "STORAGE_BACKUP_IN_PROGRESS";
pub const STORAGE_ORPHAN_SLOT_GC_IN_PROGRESS: &str = "STORAGE_ORPHAN_SLOT_GC_IN_PROGRESS";

/// Scheduled vault verification returned a non-green verdict (#1687/#1679).
///
/// Raised by `hygiene operation=vault_verify` when the restore verifier, the
/// provenance hash chain, the raw-write commitments, or the vault lineage
/// journal failed. Never downgraded to a warning — a vault that cannot prove
/// itself is the #1875 condition arriving quietly.
pub const HYGIENE_VAULT_VERIFY_FAILED: &str = "SYNAPSE_HYGIENE_VAULT_VERIFY_FAILED";

/// Scheduled vault verification could not run to completion (#2059).
///
/// Raised when a scan was refused by a resource budget — an aggregate
/// materialization ceiling, an allocation refusal, an SST page-source ceiling —
/// while every integrity predicate that *was* evaluated held. The vault is
/// unverified, which is a real deficiency and stays fail-closed, but it is not
/// the corruption alarm and must never carry the restore-from-backup
/// remediation. Conflating the two is how the alarm that must be believed gets
/// trained out of an operator.
pub const HYGIENE_VAULT_VERIFY_UNVERIFIABLE: &str = "SYNAPSE_HYGIENE_VAULT_VERIFY_UNVERIFIABLE";

// === Episodes (derived activity spans, issues #846/#847) ===
pub const EPISODE_NOT_FOUND: &str = "EPISODE_NOT_FOUND";

// === Agent spawn templates (#909) ===
pub const AGENT_TEMPLATE_NOT_FOUND: &str = "AGENT_TEMPLATE_NOT_FOUND";

// === Agent task queue (#910) ===
pub const AGENT_TASK_NOT_FOUND: &str = "AGENT_TASK_NOT_FOUND";
pub const AGENT_TASK_INVALID_TRANSITION: &str = "AGENT_TASK_INVALID_TRANSITION";

// === Models (06 section 8.7) ===
pub const MODEL_DOWNLOAD_FAILED: &str = "MODEL_DOWNLOAD_FAILED";
pub const MODEL_HASH_MISMATCH: &str = "MODEL_HASH_MISMATCH";
pub const MODEL_LOAD_FAILED: &str = "MODEL_LOAD_FAILED";
pub const MODEL_BACKEND_UNAVAILABLE: &str = "MODEL_BACKEND_UNAVAILABLE";
/// An OPTIONAL model slot is physically absent from this executable's embedded
/// model bundle (#1863).
///
/// This is not corruption and not a build defect: the slot table explicitly
/// records the model as not packaged, so the dependent capability is genuinely
/// unavailable on this build. Required models can never produce this code —
/// their absence is refused at package time.
pub const MODEL_EMBEDDED_SLOT_ABSENT: &str = "MODEL_EMBEDDED_SLOT_ABSENT";
/// The running executable carries a model bundle written by an older,
/// incompatible packager (#1863).
pub const MODEL_EMBEDDED_BUNDLE_LEGACY_FORMAT: &str = "MODEL_EMBEDDED_BUNDLE_LEGACY_FORMAT";
pub const MODEL_TOOLS_UNSUPPORTED: &str = "MODEL_TOOLS_UNSUPPORTED";
/// A local-model turn produced neither a tool call nor any message content.
///
/// This is a genuinely degenerate completion. A plain text answer (no tool call
/// but with content) is a legitimate completion and is NOT this error.
pub const MODEL_EMPTY_COMPLETION: &str = "MODEL_EMPTY_COMPLETION";
pub const MODEL_ENDPOINT_UNREACHABLE: &str = "MODEL_ENDPOINT_UNREACHABLE";
pub const MODEL_REGISTRY_NOT_FOUND: &str = "MODEL_REGISTRY_NOT_FOUND";
pub const MODEL_REGISTRY_CONFLICT: &str = "MODEL_REGISTRY_CONFLICT";
pub const MODEL_REGISTRY_DISABLED: &str = "MODEL_REGISTRY_DISABLED";
pub const MODEL_REGISTRY_UNPROBED: &str = "MODEL_REGISTRY_UNPROBED";
pub const MODEL_REGISTRY_PROBE_STALE: &str = "MODEL_REGISTRY_PROBE_STALE";
pub const MODEL_API_KEY_MISSING: &str = "MODEL_API_KEY_MISSING";
/// A stored API-key secret could not be decrypted (wrong Windows user, copied
/// database, tampered ciphertext, or non-Windows host). Never silently ignored.
pub const MODEL_API_KEY_DECRYPT_FAILED: &str = "MODEL_API_KEY_DECRYPT_FAILED";
/// Encrypting/persisting an API-key secret to the at-rest DPAPI store failed.
pub const MODEL_API_KEY_STORE_FAILED: &str = "MODEL_API_KEY_STORE_FAILED";

// === Human notifications (notify_human, issue #866) ===
pub const NOTIFY_UNSUPPORTED_PLATFORM: &str = "NOTIFY_UNSUPPORTED_PLATFORM";
pub const NOTIFY_AUMID_REGISTRATION_FAILED: &str = "NOTIFY_AUMID_REGISTRATION_FAILED";
pub const NOTIFY_DISABLED_FOR_APPLICATION: &str = "NOTIFY_DISABLED_FOR_APPLICATION";
pub const NOTIFY_DISABLED_FOR_USER: &str = "NOTIFY_DISABLED_FOR_USER";
pub const NOTIFY_DISABLED_BY_GROUP_POLICY: &str = "NOTIFY_DISABLED_BY_GROUP_POLICY";
pub const NOTIFY_DISABLED_BY_MANIFEST: &str = "NOTIFY_DISABLED_BY_MANIFEST";
pub const NOTIFY_XML_PAYLOAD_INVALID: &str = "NOTIFY_XML_PAYLOAD_INVALID";
pub const NOTIFY_SHOW_FAILED: &str = "NOTIFY_SHOW_FAILED";
pub const NOTIFY_DELIVERY_UNVERIFIED: &str = "NOTIFY_DELIVERY_UNVERIFIED";
/// A deadline-gated notification was still queued when its authority expired;
/// no platform Show call was made.
pub const NOTIFY_DELIVERY_EXPIRED: &str = "NOTIFY_DELIVERY_EXPIRED";
pub const NOTIFY_WORKER_FAILED: &str = "NOTIFY_WORKER_FAILED";

// === Safety (06 section 8.9) ===
pub const SAFETY_KILLSWITCH_ACTIVE: &str = "SAFETY_KILLSWITCH_ACTIVE";
pub const SAFETY_PROCESS_DENYLISTED: &str = "SAFETY_PROCESS_DENYLISTED";
pub const SAFETY_SHELL_DENIED_BY_POLICY: &str = "SAFETY_SHELL_DENIED_BY_POLICY";
/// Shell command requested global OS input.
///
/// Applies to `act_run_shell`/`act_run_shell_start` commands containing markers
/// such as `SendKeys`, `SendInput`, `keybd_event`, `mouse_event`, or
/// `SetForegroundWindow`, which bypass Synapse's foreground input lease and act
/// on the human foreground.
pub const SAFETY_SHELL_GLOBAL_INPUT_DENIED: &str = "SAFETY_SHELL_GLOBAL_INPUT_DENIED";
/// Shell command assigns to a PowerShell automatic/read-only variable.
///
/// PowerShell variable names are case-insensitive, so an agent-chosen name like
/// `$home` silently collides with the read-only `$HOME` automatic variable: the
/// assignment fails and any later use of the name evaluates to the operator's
/// home directory. Combined with a recursive delete this can target
/// `C:\Users\<user>` instead of a scratch path. Synapse fails closed on the
/// assignment before the command reaches the OS (#1507).
pub const SAFETY_SHELL_RESERVED_VARIABLE_COLLISION: &str =
    "SAFETY_SHELL_RESERVED_VARIABLE_COLLISION";
/// Recursive delete/move whose target cannot be proven contained in the active
/// workspace.
///
/// Applies to `Remove-Item`/`rm`/`rd`/`Move-Item` (and `cmd` `del /s` / `rmdir
/// /s`) used recursively against a target that resolves to — or is derived from
/// an automatic variable that can resolve to — a path outside the shell job's
/// working directory, such as the user home, a drive root, or the Windows/
/// tooling directories. Synapse refuses rather than run an unbounded recursive
/// delete it cannot prove safe (#1507).
pub const SAFETY_SHELL_RECURSIVE_DELETE_UNCONTAINED: &str =
    "SAFETY_SHELL_RECURSIVE_DELETE_UNCONTAINED";
pub const SAFETY_LAUNCH_DENIED_BY_POLICY: &str = "SAFETY_LAUNCH_DENIED_BY_POLICY";
pub const SAFETY_SECRET_REDACTED: &str = "SAFETY_SECRET_REDACTED";
pub const SAFETY_PERMISSION_DENIED: &str = "SAFETY_PERMISSION_DENIED";
pub const SAFETY_PROFILE_ACTION_DENIED: &str = "SAFETY_PROFILE_ACTION_DENIED";
