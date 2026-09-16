#[cfg(windows)]
use super::error::map_vigem_error;
use super::reports::{
    Ds4ReportSnapshot, X360ReportSnapshot, ds4_report_snapshot, x360_report_snapshot,
};
use super::*;
#[cfg(not(windows))]
use synapse_core::{Action, AimCurve, Backend, MouseTarget, Point};
use synapse_core::{ButtonAction, GamepadController, GamepadReport, PadButton, Stick, Trigger};

#[test]
fn x360_report_snapshot_maps_buttons_axes_and_triggers() {
    let report = GamepadReport {
        controller: GamepadController::X360,
        buttons: vec![
            PadButton::A,
            PadButton::Start,
            PadButton::Lb,
            PadButton::A,
            PadButton::Guide,
        ],
        thumb_l: (1.0, -1.0),
        thumb_r: (0.5, -0.5),
        lt: 1.0,
        rt: 0.5,
    };
    let before = "buttons=[a,start,lb,a,guide] thumb_l=(1,-1) thumb_r=(0.5,-0.5) lt=1 rt=0.5";
    let after = x360_report_snapshot(&report);
    println!("readback=vigem_x360_report edge=happy before={before} after={after:?}");
    assert_eq!(
        after,
        X360ReportSnapshot {
            buttons_raw: 0x1510,
            left_trigger: 255,
            right_trigger: 128,
            thumb_lx: 32_767,
            thumb_ly: -32_768,
            thumb_rx: 16_384,
            thumb_ry: -16_384,
        }
    );
}

#[test]
fn x360_report_snapshot_clamps_invalid_numeric_edges() {
    let report = GamepadReport {
        controller: GamepadController::X360,
        buttons: vec![PadButton::Down, PadButton::Right],
        thumb_l: (1.5, -2.0),
        thumb_r: (f32::NAN, f32::INFINITY),
        lt: 2.0,
        rt: f32::NAN,
    };
    let before = "buttons=[down,right] thumb_l=(1.5,-2.0) thumb_r=(NaN,inf) lt=2.0 rt=NaN";
    let after = x360_report_snapshot(&report);
    println!("readback=vigem_x360_report edge=clamp before={before} after={after:?}");
    assert_eq!(
        after,
        X360ReportSnapshot {
            buttons_raw: 0x000a,
            left_trigger: 255,
            right_trigger: 0,
            thumb_lx: 32_767,
            thumb_ly: -32_768,
            thumb_rx: 0,
            thumb_ry: 0,
        }
    );
}

#[test]
fn ds4_report_snapshot_maps_buttons_axes_triggers_and_specials() {
    let report = GamepadReport {
        controller: GamepadController::Ds4,
        buttons: vec![
            PadButton::A,
            PadButton::B,
            PadButton::X,
            PadButton::Y,
            PadButton::Lb,
            PadButton::Rb,
            PadButton::Ls,
            PadButton::Rs,
            PadButton::Back,
            PadButton::Start,
            PadButton::Guide,
            PadButton::Up,
            PadButton::Right,
        ],
        thumb_l: (1.0, -1.0),
        thumb_r: (0.0, 0.5),
        lt: 0.25,
        rt: 1.0,
    };
    let before = "controller=ds4 buttons=[a,b,x,y,lb,rb,ls,rs,back,start,guide,up,right] thumb_l=(1,-1) thumb_r=(0,0.5) lt=0.25 rt=1";
    let after = ds4_report_snapshot(&report);
    println!("readback=vigem_ds4_report edge=happy before={before} after={after:?}");
    assert_eq!(
        after,
        Ds4ReportSnapshot {
            buttons: 0xfff1,
            special: 0x01,
            trigger_l: 64,
            trigger_r: 255,
            thumb_lx: 255,
            thumb_ly: 255,
            thumb_rx: 128,
            thumb_ry: 64,
        }
    );
}

#[test]
fn ds4_report_snapshot_clamps_invalid_numeric_and_dpad_edges() {
    let report = GamepadReport {
        controller: GamepadController::Ds4,
        buttons: vec![
            PadButton::Up,
            PadButton::Down,
            PadButton::Left,
            PadButton::Right,
        ],
        thumb_l: (f32::NAN, f32::INFINITY),
        thumb_r: (-2.0, 2.0),
        lt: f32::NAN,
        rt: 2.0,
    };
    let before =
        "controller=ds4 buttons=[up,down,left,right] thumb_l=(NaN,inf) thumb_r=(-2,2) lt=NaN rt=2";
    let after = ds4_report_snapshot(&report);
    println!("readback=vigem_ds4_report edge=clamp before={before} after={after:?}");
    assert_eq!(
        after,
        Ds4ReportSnapshot {
            buttons: 0x0808,
            special: 0,
            trigger_l: 0,
            trigger_r: 255,
            thumb_lx: 128,
            thumb_ly: 128,
            thumb_rx: 0,
            thumb_ry: 0,
        }
    );
}

#[test]
fn pad_state_helpers_track_partial_updates_and_neutral_removal() {
    let mut state = EmitState::new();
    let before = state.snapshot();
    println!("readback=vigem_pad_state edge=partial before={before:?}");
    apply_pad_button(&mut state, 3, PadButton::B, ButtonAction::Down);
    apply_pad_stick(&mut state, 3, Stick::Left, 0.25, -0.75);
    apply_pad_trigger(&mut state, 3, Trigger::Right, 0.5);
    let after_down = state.snapshot();
    println!("readback=vigem_pad_state edge=partial after_down={after_down:?}");
    assert_eq!(after_down.pad_state[&3].buttons, vec![PadButton::B]);
    assert_eq!(after_down.pad_state[&3].thumb_l, (0.25, -0.75));
    assert!((after_down.pad_state[&3].rt - 0.5).abs() < f32::EPSILON);

    apply_pad_button(&mut state, 3, PadButton::B, ButtonAction::Up);
    apply_pad_stick(&mut state, 3, Stick::Left, 0.0, 0.0);
    apply_pad_trigger(&mut state, 3, Trigger::Right, 0.0);
    let after_neutral = state.snapshot();
    println!("readback=vigem_pad_state edge=partial after_neutral={after_neutral:?}");
    assert!(!after_neutral.pad_state.contains_key(&3));
}

#[cfg(not(windows))]
#[test]
fn non_windows_backend_fails_closed_without_state_mutation() {
    let backend = VigemBackend::new();
    let mut state = EmitState::new();
    let before = state.snapshot();
    println!("readback=vigem_non_windows edge=pad_report before={before:?}");
    let result = backend.execute(
        &Action::PadReport {
            pad: 1,
            report: GamepadReport {
                controller: GamepadController::X360,
                buttons: vec![PadButton::A],
                thumb_l: (0.0, 0.0),
                thumb_r: (0.0, 0.0),
                lt: 0.0,
                rt: 0.0,
            },
        },
        &mut state,
    );
    let after = state.snapshot();
    let error = result
        .err()
        .unwrap_or_else(|| panic!("non-Windows ViGEm pad report must fail closed"));
    println!(
        "readback=vigem_non_windows edge=pad_report after={after:?} after_code={}",
        error.code()
    );
    assert_eq!(
        error.code(),
        synapse_core::error_codes::ACTION_BACKEND_UNAVAILABLE
    );
    assert_eq!(before, after);
}

#[cfg(not(windows))]
#[test]
fn non_windows_ensure_ready_and_non_pad_edges_fail_closed() {
    let backend = VigemBackend::new();
    let ensure_error = backend
        .ensure_ready()
        .err()
        .unwrap_or_else(|| panic!("non-Windows ensure_ready must fail closed"));
    println!(
        "readback=vigem_non_windows edge=ensure_ready before=platform:not_windows after_code={} after_detail={:?}",
        ensure_error.code(),
        ensure_error.detail()
    );
    assert_eq!(
        ensure_error.code(),
        synapse_core::error_codes::ACTION_BACKEND_UNAVAILABLE
    );

    let mut state = EmitState::new();
    let before = state.snapshot();
    let non_pad = Action::MouseMove {
        to: MouseTarget::Screen {
            point: Point { x: 1, y: 2 },
        },
        curve: AimCurve::Instant,
        duration_ms: 0,
        backend: Backend::Vigem,
    };
    let result = backend.execute(&non_pad, &mut state);
    let after = state.snapshot();
    let error = result
        .err()
        .unwrap_or_else(|| panic!("non-Windows non-pad action must fail closed"));
    println!(
        "readback=vigem_non_windows edge=non_pad before={before:?} after={after:?} after_code={}",
        error.code()
    );
    assert_eq!(
        error.code(),
        synapse_core::error_codes::ACTION_BACKEND_UNAVAILABLE
    );
    assert_eq!(before, after);
}

#[cfg(not(windows))]
#[test]
fn non_windows_empty_release_all_is_noop_but_non_empty_pad_state_fails() {
    let backend = VigemBackend::new();
    let mut empty = EmitState::new();
    let before_empty = empty.snapshot();
    let empty_result = backend.execute(&Action::ReleaseAll, &mut empty);
    let after_empty = empty.snapshot();
    println!(
        "readback=vigem_non_windows edge=empty_release before={before_empty:?} after={after_empty:?} result={empty_result:?}"
    );
    assert!(empty_result.is_ok());
    assert_eq!(before_empty, after_empty);

    let mut seeded = EmitState::new();
    apply_pad_report(
        &mut seeded,
        2,
        GamepadReport {
            controller: GamepadController::X360,
            buttons: vec![PadButton::Y],
            thumb_l: (0.0, 0.0),
            thumb_r: (0.0, 0.0),
            lt: 0.0,
            rt: 0.0,
        },
    );
    let before_seeded = seeded.snapshot();
    let seeded_result = backend.execute(&Action::ReleaseAll, &mut seeded);
    let after_seeded = seeded.snapshot();
    let error = seeded_result
        .err()
        .unwrap_or_else(|| panic!("non-empty non-Windows release_all must fail closed"));
    println!(
        "readback=vigem_non_windows edge=non_empty_release before={before_seeded:?} after={after_seeded:?} after_code={}",
        error.code()
    );
    assert_eq!(
        error.code(),
        synapse_core::error_codes::ACTION_BACKEND_UNAVAILABLE
    );
    assert_eq!(before_seeded, after_seeded);
}

#[cfg(windows)]
#[test]
fn vigem_error_mapping_preserves_declared_codes() {
    let not_installed = map_vigem_error("connect_vigembus", vigem_client::Error::BusNotFound);
    println!(
        "readback=vigem_error_mapping edge=bus_missing after_code={} after_detail={:?}",
        not_installed.code(),
        not_installed.detail()
    );
    assert_eq!(
        not_installed.code(),
        synapse_core::error_codes::ACTION_VIGEM_NOT_INSTALLED
    );

    let plugin_failed = map_vigem_error("plugin_x360_target", vigem_client::Error::NoFreeSlot);
    println!(
        "readback=vigem_error_mapping edge=plugin_failed after_code={} after_detail={:?}",
        plugin_failed.code(),
        plugin_failed.detail()
    );
    assert_eq!(
        plugin_failed.code(),
        synapse_core::error_codes::ACTION_VIGEM_PLUGIN_FAILED
    );
}
