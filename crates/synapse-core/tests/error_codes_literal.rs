use std::collections::BTreeMap;

macro_rules! assert_literal {
    ($name:ident) => {
        assert_eq!(synapse_core::error_codes::$name, stringify!($name));
    };
}

#[test]
fn error_codes_match_literal_names() {
    assert_perception_error_codes();
    assert_action_error_codes();
    assert_reflex_profile_session_storage_error_codes();
    assert_model_and_safety_error_codes();
}

fn assert_perception_error_codes() {
    assert_literal!(OBSERVE_NO_PERCEPTION_AVAILABLE);
    assert_literal!(OBSERVE_INTERNAL);
    assert_literal!(CAPTURE_GRAPHICS_API_UNSUPPORTED);
    assert_literal!(CAPTURE_PRINTWINDOW_DISABLED);
    assert_literal!(CAPTURE_PRINTWINDOW_BLACK);
    assert_literal!(CAPTURE_TARGET_LOST);
    assert_literal!(CAPTURE_NO_DIRTY_REGIONS);
    assert_literal!(A11Y_NOT_AVAILABLE);
    assert_literal!(A11Y_ELEMENT_STALE);
    assert_literal!(A11Y_NO_FOREGROUND);
    assert_literal!(A11Y_CDP_UNREACHABLE);
    assert_literal!(A11Y_CDP_ATTACH_FAILED);
    assert_literal!(A11Y_CDP_AXTREE_FAILED);
    assert_literal!(A11Y_CDP_EXTENSION_UNAVAILABLE);
    assert_literal!(A11Y_CDP_EXTENSION_DETACHED);
    assert_literal!(A11Y_CDP_EXTENSION_TIMEOUT);
    assert_literal!(A11Y_CDP_DEBUGGER_WARNING_UNSUPPRESSED);
    assert_literal!(CHROME_BRIDGE_EXTENSION_STALE);
    assert_literal!(BROWSER_URL_SCHEME_UNSUPPORTED);
    assert_literal!(BROWSER_NAVIGATION_FAILED);
    assert_literal!(A11Y_UIA_WORKER_TIMEOUT);
    assert_literal!(A11Y_TARGET_WINDOW_MINIMIZED_UIA_UNAVAILABLE);
    assert_literal!(DETECTION_MODEL_NOT_LOADED);
    assert_literal!(DETECTION_MODEL_INFER_FAILED);
    assert_literal!(DETECTION_NO_FRAME);
    assert_literal!(OCR_NO_TEXT);
    assert_literal!(OCR_BACKEND_UNAVAILABLE);
    assert_literal!(HUD_NO_ACTIVE_PROFILE);
    assert_literal!(HUD_FIELD_NOT_DEFINED);
    assert_literal!(HUD_EXTRACTION_FAILED);
    assert_literal!(AUDIO_DEVICE_LOST);
    assert_literal!(AUDIO_LOOPBACK_INIT_FAILED);
    assert_literal!(AUDIO_STT_MODEL_NOT_LOADED);
}

fn assert_action_error_codes() {
    assert_literal!(ACTION_QUEUE_FULL);
    assert_literal!(ACTION_RATE_LIMITED);
    assert_literal!(ACTION_BACKEND_UNAVAILABLE);
    assert_literal!(ACTION_TARGET_INVALID);
    assert_literal!(ACTION_HOLD_EXCEEDED_MAX);
    assert_literal!(ACTION_VIGEM_NOT_INSTALLED);
    assert_literal!(ACTION_VIGEM_PLUGIN_FAILED);
    assert_literal!(ACTION_ELEMENT_NOT_RESOLVED);
    assert_literal!(ACTION_ELEMENT_PATTERN_UNSUPPORTED);
    assert_literal!(TRANSIENT_ELEMENT_EXPIRED);
    assert_literal!(ACTION_FOREGROUND_LOST);
    assert_literal!(ACTION_NO_OBSERVED_DELTA);
    assert_literal!(ACTION_VERIFY_SURFACE_UNAVAILABLE);
    assert_literal!(ACTION_POSTCONDITION_FAILED);
    assert_literal!(ACTION_UNSUPPORTED_KEY);
    assert_literal!(ACTION_DRAG_DISTANCE_EXCEEDS_LIMIT);
    assert_literal!(STUCK_KEY_AUTO_RELEASED);
    assert_literal!(SAFETY_RELEASE_ALL_FIRED);
    assert_literal!(SAFETY_OPERATOR_HOTKEY_FIRED);
    assert_literal!(ACTION_FOREGROUND_LEASE_BUSY);
    assert_literal!(ACTION_FOREGROUND_LEASE_NOT_HELD);
    assert_literal!(FOREGROUND_ACTIVATION_REFUSED);
    assert_literal!(ACTION_FOREGROUND_CONTEXT_CAPTURE_FAILED);
    assert_literal!(ACTION_FOREGROUND_CONTEXT_RESTORE_FAILED);
    assert_literal!(ACTION_FOREGROUND_CONTEXT_RESTORE_SKIPPED);
    assert_literal!(FOREGROUND_RESTORE_SKIPPED_HUMAN_MOVED);
    assert_literal!(ACTION_ELEMENT_VALUE_READ_ONLY);
}

fn assert_reflex_profile_session_storage_error_codes() {
    assert_literal!(REFLEX_CAP_REACHED);
    assert_literal!(REFLEX_KIND_INVALID);
    assert_literal!(REFLEX_PARAMS_INVALID);
    assert_literal!(REFLEX_TARGET_INVALID);
    assert_literal!(REFLEX_FILTER_INVALID);
    assert_literal!(REFLEX_PRIORITY_INVALID);
    assert_literal!(REFLEX_TICK_LATE);
    assert_literal!(REFLEX_TRACK_LOST);
    assert_literal!(REFLEX_STARVED);
    assert_literal!(REFLEX_DISABLED_BY_OPERATOR);
    assert_literal!(REFLEX_LIFETIME_EXPIRED);
    assert_literal!(REFLEX_RECURSION_LIMIT);
    assert_literal!(REFLEX_ACTION_PERMISSION_DENIED);
    assert_literal!(REFLEX_DEBOUNCED);

    assert_literal!(PROFILE_NOT_FOUND);
    assert_literal!(PROFILE_PARSE_ERROR);
    assert_literal!(PROFILE_VERSION_INCOMPATIBLE);
    assert_literal!(PROFILE_KEYMAP_INVALID);
    assert_literal!(PROFILE_HUD_REGION_INVALID);
    assert_literal!(CAPTURE_TARGET_INVALID);
    assert_literal!(PERCEPTION_MODE_INVALID);

    assert_literal!(SESSION_NOT_FOUND);
    assert_literal!(SESSION_EXPIRED);
    assert_literal!(SUBSCRIPTION_NOT_FOUND);
    assert_literal!(SUBSCRIPTION_CAP_REACHED);
    assert_literal!(TOOL_NOT_FOUND);
    assert_literal!(TOOL_PARAMS_INVALID);
    assert_literal!(TOOL_INTERNAL_ERROR);
    assert_literal!(HTTP_BIND_NON_LOOPBACK_REFUSED);
    assert_literal!(HTTP_TOKEN_INVALID);
    assert_literal!(HTTP_ORIGIN_REFUSED);
    assert_literal!(HTTP_SESSION_INVALID);
    assert_literal!(REPLAY_TARGET_INVALID);
    assert_literal!(REPLAY_FORMAT_INVALID);

    assert_literal!(STORAGE_OPEN_FAILED);
    assert_literal!(STORAGE_WRITE_FAILED);
    assert_literal!(STORAGE_READ_FAILED);
    assert_literal!(STORAGE_CORRUPTED);
    assert_literal!(STORAGE_SCHEMA_MISMATCH);
    assert_literal!(STORAGE_DISK_PRESSURE_LEVEL_1);
    assert_literal!(STORAGE_DISK_PRESSURE_LEVEL_2);
    assert_literal!(STORAGE_DISK_PRESSURE_LEVEL_3);
    assert_literal!(STORAGE_DISK_PRESSURE_LEVEL_4);
    assert_literal!(STORAGE_CF_HARD_CAP_REACHED);

    assert_literal!(EPISODE_NOT_FOUND);
}

fn assert_model_and_safety_error_codes() {
    assert_literal!(MODEL_DOWNLOAD_FAILED);
    assert_literal!(MODEL_HASH_MISMATCH);
    assert_literal!(MODEL_LOAD_FAILED);
    assert_literal!(MODEL_BACKEND_UNAVAILABLE);
    assert_literal!(MODEL_TOOLS_UNSUPPORTED);
    assert_literal!(MODEL_EMPTY_COMPLETION);
    assert_literal!(MODEL_ENDPOINT_UNREACHABLE);
    assert_literal!(MODEL_REGISTRY_NOT_FOUND);
    assert_literal!(MODEL_REGISTRY_CONFLICT);
    assert_literal!(MODEL_REGISTRY_DISABLED);
    assert_literal!(MODEL_REGISTRY_UNPROBED);
    assert_literal!(MODEL_REGISTRY_PROBE_STALE);

    assert_literal!(SAFETY_KILLSWITCH_ACTIVE);
    assert_literal!(SAFETY_PROCESS_DENYLISTED);
    assert_literal!(SAFETY_SHELL_DENIED_BY_POLICY);
    assert_literal!(SAFETY_SHELL_GLOBAL_INPUT_DENIED);
    assert_literal!(SAFETY_SHELL_RESERVED_VARIABLE_COLLISION);
    assert_literal!(SAFETY_SHELL_RECURSIVE_DELETE_UNCONTAINED);
    assert_literal!(SAFETY_LAUNCH_DENIED_BY_POLICY);
    assert_literal!(SAFETY_SECRET_REDACTED);
    assert_literal!(SAFETY_PERMISSION_DENIED);
    assert_literal!(SAFETY_PROFILE_ACTION_DENIED);
}

#[test]
fn m3_error_codes_snapshot_with_readback() {
    let codes = [
        (
            "REFLEX_RECURSION_LIMIT",
            synapse_core::error_codes::REFLEX_RECURSION_LIMIT,
        ),
        (
            "REFLEX_ACTION_PERMISSION_DENIED",
            synapse_core::error_codes::REFLEX_ACTION_PERMISSION_DENIED,
        ),
        (
            "REFLEX_DEBOUNCED",
            synapse_core::error_codes::REFLEX_DEBOUNCED,
        ),
        (
            "HTTP_BIND_NON_LOOPBACK_REFUSED",
            synapse_core::error_codes::HTTP_BIND_NON_LOOPBACK_REFUSED,
        ),
        (
            "HTTP_TOKEN_INVALID",
            synapse_core::error_codes::HTTP_TOKEN_INVALID,
        ),
        (
            "HTTP_ORIGIN_REFUSED",
            synapse_core::error_codes::HTTP_ORIGIN_REFUSED,
        ),
        (
            "HTTP_SESSION_INVALID",
            synapse_core::error_codes::HTTP_SESSION_INVALID,
        ),
        (
            "STORAGE_DISK_PRESSURE_LEVEL_1",
            synapse_core::error_codes::STORAGE_DISK_PRESSURE_LEVEL_1,
        ),
        (
            "STORAGE_DISK_PRESSURE_LEVEL_2",
            synapse_core::error_codes::STORAGE_DISK_PRESSURE_LEVEL_2,
        ),
        (
            "STORAGE_DISK_PRESSURE_LEVEL_3",
            synapse_core::error_codes::STORAGE_DISK_PRESSURE_LEVEL_3,
        ),
        (
            "STORAGE_DISK_PRESSURE_LEVEL_4",
            synapse_core::error_codes::STORAGE_DISK_PRESSURE_LEVEL_4,
        ),
        (
            "STORAGE_CF_HARD_CAP_REACHED",
            synapse_core::error_codes::STORAGE_CF_HARD_CAP_REACHED,
        ),
        (
            "REPLAY_TARGET_INVALID",
            synapse_core::error_codes::REPLAY_TARGET_INVALID,
        ),
        (
            "REPLAY_FORMAT_INVALID",
            synapse_core::error_codes::REPLAY_FORMAT_INVALID,
        ),
        (
            "SAFETY_PERMISSION_DENIED",
            synapse_core::error_codes::SAFETY_PERMISSION_DENIED,
        ),
        (
            "SAFETY_PROFILE_ACTION_DENIED",
            synapse_core::error_codes::SAFETY_PROFILE_ACTION_DENIED,
        ),
    ];
    let expected = codes.iter().map(|(name, _value)| *name).collect::<Vec<_>>();
    println!("readback=m3_error_codes before=expected:{expected:?}");

    let actual = codes
        .into_iter()
        .map(|(name, value)| {
            assert_eq!(value, name);
            (name, value)
        })
        .collect::<BTreeMap<_, _>>();
    println!(
        "readback=m3_error_codes after=actual:{actual:?} final_count:{}",
        actual.len()
    );
    insta::assert_json_snapshot!("m3_error_codes", actual);
}
