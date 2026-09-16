use synapse_action::{
    BackendResolutionPolicy, ResolvedBackend, resolve_backend, resolve_backend_with_policy,
};
use synapse_core::{Action, Backend, ButtonAction, Key, KeyCode, MouseButton, PadButton};

#[test]
fn explicit_backend_variants_resolve_or_fail_closed() {
    let action = key_down_action();

    assert_backend(
        "explicit_software",
        Backend::Software,
        &action,
        ResolvedBackend::Software,
    );
    assert_backend(
        "explicit_vigem",
        Backend::Vigem,
        &action,
        ResolvedBackend::Vigem,
    );

    assert_backend(
        "explicit_hardware",
        Backend::Hardware,
        &action,
        ResolvedBackend::Hardware,
    );

    assert_backend(
        "explicit_auto_keyboard",
        Backend::Auto,
        &action,
        ResolvedBackend::Software,
    );
}

#[test]
fn auto_backend_routes_keyboard_mouse_and_pad_actions() {
    assert_backend(
        "auto_keyboard",
        Backend::Auto,
        &key_down_action(),
        ResolvedBackend::Software,
    );
    assert_backend(
        "auto_mouse",
        Backend::Auto,
        &mouse_button_action(),
        ResolvedBackend::Software,
    );
    assert_backend(
        "auto_pad",
        Backend::Auto,
        &pad_button_action(),
        ResolvedBackend::Vigem,
    );
}

#[test]
fn default_policy_preserves_m2_auto_resolution_table() {
    let policy = BackendResolutionPolicy::default();

    assert_backend_with_policy(
        "default_policy_keyboard",
        Backend::Auto,
        &key_down_action(),
        policy,
        ResolvedBackend::Software,
    );
    assert_backend_with_policy(
        "default_policy_mouse",
        Backend::Auto,
        &mouse_button_action(),
        policy,
        ResolvedBackend::Software,
    );
    assert_backend_with_policy(
        "default_policy_pad",
        Backend::Auto,
        &pad_button_action(),
        policy,
        ResolvedBackend::Vigem,
    );
    assert_backend_with_policy(
        "default_policy_release_all",
        Backend::Auto,
        &Action::ReleaseAll,
        policy,
        ResolvedBackend::Software,
    );
}

#[test]
fn profile_default_hardware_policy_routes_auto_to_hardware() {
    let policy = BackendResolutionPolicy {
        default_backend: Backend::Hardware,
        keyboard_default: Backend::Auto,
        mouse_default: Backend::Auto,
        pad_default: Backend::Auto,
    };

    assert_backend_with_policy(
        "profile_hardware_keyboard",
        Backend::Auto,
        &key_down_action(),
        policy,
        ResolvedBackend::Hardware,
    );
    assert_backend_with_policy(
        "profile_hardware_mouse",
        Backend::Auto,
        &mouse_button_action(),
        policy,
        ResolvedBackend::Hardware,
    );
    assert_backend_with_policy(
        "profile_hardware_pad",
        Backend::Auto,
        &pad_button_action(),
        policy,
        ResolvedBackend::Hardware,
    );
    assert_backend_with_policy(
        "profile_hardware_release_all",
        Backend::Auto,
        &Action::ReleaseAll,
        policy,
        ResolvedBackend::Hardware,
    );
}

#[test]
fn class_defaults_override_profile_default_for_auto_only() {
    let policy = BackendResolutionPolicy {
        default_backend: Backend::Hardware,
        keyboard_default: Backend::Software,
        mouse_default: Backend::Auto,
        pad_default: Backend::Vigem,
    };

    assert_backend_with_policy(
        "class_keyboard_software",
        Backend::Auto,
        &key_down_action(),
        policy,
        ResolvedBackend::Software,
    );
    assert_backend_with_policy(
        "class_pad_vigem",
        Backend::Auto,
        &pad_button_action(),
        policy,
        ResolvedBackend::Vigem,
    );
    assert_backend_with_policy(
        "explicit_software_ignores_profile_hardware",
        Backend::Software,
        &key_down_action(),
        policy,
        ResolvedBackend::Software,
    );
}

fn assert_backend(edge: &str, requested: Backend, action: &Action, expected: ResolvedBackend) {
    let resolved = resolve_backend(requested, action)
        .unwrap_or_else(|err| panic!("{edge} should resolve backend, got {err}"));
    assert_eq!(resolved, expected);
    println!(
        "readback=backend_resolution edge={edge} before_backend={requested:?} after_backend={} result_value={:?}",
        resolved.as_str(),
        resolved
    );
}

fn assert_backend_with_policy(
    edge: &str,
    requested: Backend,
    action: &Action,
    policy: BackendResolutionPolicy,
    expected: ResolvedBackend,
) {
    let resolved = resolve_backend_with_policy(requested, action, policy)
        .unwrap_or_else(|err| panic!("{edge} should resolve backend, got {err}"));
    assert_eq!(resolved, expected);
    println!(
        "readback=backend_resolution edge={edge} policy={policy:?} before_backend={requested:?} after_backend={} result_value={resolved:?}",
        resolved.as_str()
    );
}

fn key_down_action() -> Action {
    Action::KeyDown {
        key: Key {
            code: KeyCode::Named {
                value: "a".to_owned(),
            },
            use_scancode: false,
        },
        backend: Backend::Auto,
    }
}

const fn mouse_button_action() -> Action {
    Action::MouseButton {
        button: MouseButton::Left,
        action: ButtonAction::Down,
        hold_ms: 0,
        backend: Backend::Auto,
    }
}

const fn pad_button_action() -> Action {
    Action::PadButton {
        pad: 0,
        button: PadButton::A,
        action: ButtonAction::Down,
        hold_ms: 0,
    }
}
