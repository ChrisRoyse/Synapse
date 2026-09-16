use synapse_action::ActionError;
use synapse_core::error_codes;

const DETAIL: &str = "synthetic detail";

macro_rules! case {
    ($edge:literal, RateLimited, $code:ident) => {
        (
            $edge,
            ActionError::RateLimited {
                detail: DETAIL.to_owned(),
                retry_after_ms: 1,
            },
            error_codes::$code,
        )
    };
    ($edge:literal, $variant:ident, $code:ident) => {
        (
            $edge,
            ActionError::$variant {
                detail: DETAIL.to_owned(),
            },
            error_codes::$code,
        )
    };
}

#[test]
fn action_error_codes_match_core_literals() {
    for (variant, error, expected) in action_error_cases() {
        let before = format!("{error:?}");
        let after = error.code();
        assert_eq!(after, expected);
        assert!(!error.detail().is_empty());
        println!("readback=error_codes_match edge={variant} before={before} after={after}");
    }
}

#[test]
fn every_action_error_variant_carries_detail() {
    for (variant, error, _expected) in action_error_cases() {
        assert_eq!(error.detail(), DETAIL);
        println!("readback=action_error_detail edge={variant} result_value={DETAIL}");
    }
}

fn action_error_cases() -> Vec<(&'static str, ActionError, &'static str)> {
    vec![
        case!("queue_full", QueueFull, ACTION_QUEUE_FULL),
        case!("rate_limited", RateLimited, ACTION_RATE_LIMITED),
        case!(
            "backend_unavailable",
            BackendUnavailable,
            ACTION_BACKEND_UNAVAILABLE
        ),
        case!(
            "foreground_activation_refused",
            ForegroundActivationRefused,
            FOREGROUND_ACTIVATION_REFUSED
        ),
        case!("target_invalid", TargetInvalid, ACTION_TARGET_INVALID),
        case!(
            "hold_exceeded_max",
            HoldExceededMax,
            ACTION_HOLD_EXCEEDED_MAX
        ),
        case!(
            "vigem_not_installed",
            VigemNotInstalled,
            ACTION_VIGEM_NOT_INSTALLED
        ),
        case!(
            "vigem_plugin_failed",
            VigemPluginFailed,
            ACTION_VIGEM_PLUGIN_FAILED
        ),
        case!(
            "element_not_resolved",
            ElementNotResolved,
            ACTION_ELEMENT_NOT_RESOLVED
        ),
        case!("foreground_lost", ForegroundLost, ACTION_FOREGROUND_LOST),
        case!("unsupported_key", UnsupportedKey, ACTION_UNSUPPORTED_KEY),
        case!(
            "drag_distance_exceeds_limit",
            DragDistanceExceedsLimit,
            ACTION_DRAG_DISTANCE_EXCEEDS_LIMIT
        ),
        case!(
            "stuck_key_auto_released",
            StuckKeyAutoReleased,
            STUCK_KEY_AUTO_RELEASED
        ),
        case!(
            "safety_release_all_fired",
            SafetyReleaseAllFired,
            SAFETY_RELEASE_ALL_FIRED
        ),
        case!(
            "safety_operator_hotkey_fired",
            SafetyOperatorHotkeyFired,
            SAFETY_OPERATOR_HOTKEY_FIRED
        ),
    ]
}
