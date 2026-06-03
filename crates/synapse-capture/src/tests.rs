use std::sync::Mutex;
// `Ordering`, `thread`, and `Duration` are only used by the Windows-only capture
// loop tests below (real capture does not exist off Windows after the synthetic
// mock removal).
#[cfg(windows)]
use std::{sync::atomic::Ordering, thread, time::Duration};

use proptest::prelude::*;
use synapse_core::{Point, error_codes};

use super::*;

static ENV_LOCK: Mutex<()> = Mutex::new(());
// Only the Windows-only capture loop tests serialize on this lock.
#[cfg(windows)]
static CAPTURE_LOCK: Mutex<()> = Mutex::new(());

// The previous `captured_frame_synthetic_shape_is_stable` and
// `captured_frame_drop_loop_is_raii_safe_for_synthetic_texture` tests exercised
// the non-Windows `CapturedFrame::synthetic` mock, which has been removed. The
// non-Windows capture path is now covered by the fail-loud test in
// `platform::non_windows::tests` (it asserts `GraphicsApiUnsupported` instead of
// fabricating frames).

#[cfg(windows)]
#[test]
fn captured_frame_drop_loop_queries_d3d_texture() -> Result<(), CaptureError> {
    use windows::Win32::Graphics::Direct3D11::D3D11_TEXTURE2D_DESC;

    let _guard = CAPTURE_LOCK
        .lock()
        .unwrap_or_else(|err| panic!("capture lock poisoned: {err}"));
    let handle = spawn_capture_loop(CaptureConfig {
        min_update_interval_ms: 16,
        dirty_region_only: false,
        ..CaptureConfig::default()
    })?;
    let rx = handle.receiver();
    let mut queried = 0_u32;

    for _ in 0..1_000 {
        let frame =
            rx.recv_timeout(Duration::from_secs(5))
                .map_err(|err| CaptureError::ThreadFailed {
                    detail: err.to_string(),
                })?;
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe {
            frame.texture.get().GetDesc(std::ptr::addr_of_mut!(desc));
        }
        if queried == 0 || queried == 999 {
            println!(
                "d3d_query frame_seq={} desc_width={} desc_height={} frame_width={} frame_height={}",
                frame.frame_seq, desc.Width, desc.Height, frame.width, frame.height
            );
        }
        assert_eq!(desc.Width, frame.width);
        assert_eq!(desc.Height, frame.height);
        queried = queried.saturating_add(1);
    }

    let stats = handle.stats();
    println!(
        "after d3d_drop_loop queried={} captured={} dropped={} priority={:?}",
        queried,
        stats.frames_captured(),
        stats.frames_dropped(),
        stats.thread_priority()
    );
    handle.stop()?;
    assert_eq!(queried, 1_000);
    Ok(())
}

#[test]
fn force_dxgi_env_value_selects_dxgi_backend() {
    let config = CaptureConfig {
        backend_preference: CaptureBackendPreference::from_force_dxgi_value(Some("1")),
        ..CaptureConfig::default()
    };
    assert_eq!(config.selected_backend(), CaptureBackend::DxgiDuplication);
}

#[test]
fn force_dxgi_env_var_selects_dxgi_backend() {
    let _guard = ENV_LOCK
        .lock()
        .unwrap_or_else(|err| panic!("env lock poisoned: {err}"));
    let previous = std::env::var_os("SYNAPSE_CAPTURE_FORCE_DXGI");
    println!(
        "before env_dxgi previous={:?} selected_backend={:?}",
        previous,
        CaptureConfig::default().selected_backend()
    );

    // SAFETY: this test serializes access with ENV_LOCK and restores the
    // prior value before returning.
    unsafe {
        std::env::set_var("SYNAPSE_CAPTURE_FORCE_DXGI", "1");
    }
    let config = CaptureConfig::default().with_env_backend();
    println!(
        "after env_dxgi value=1 selected_backend={:?}",
        config.selected_backend()
    );
    assert_eq!(config.selected_backend(), CaptureBackend::DxgiDuplication);

    // SAFETY: same ENV_LOCK serialization as above.
    unsafe {
        match previous {
            Some(value) => std::env::set_var("SYNAPSE_CAPTURE_FORCE_DXGI", value),
            None => std::env::remove_var("SYNAPSE_CAPTURE_FORCE_DXGI"),
        }
    }
}

#[test]
fn auto_backend_falls_back_only_for_graphics_unsupported() {
    let unsupported = CaptureError::GraphicsApiUnsupported {
        detail: "synthetic unsupported".to_owned(),
    };
    println!(
        "before fallback preference={:?} error_code={}",
        CaptureBackendPreference::Auto,
        unsupported.code()
    );
    assert!(should_fallback_to_dxgi(
        CaptureBackendPreference::Auto,
        &unsupported
    ));
    assert_eq!(
        backend_after_fallback(CaptureBackendPreference::Auto, &unsupported),
        CaptureBackend::DxgiDuplication
    );
    println!(
        "after fallback effective_backend={:?}",
        backend_after_fallback(CaptureBackendPreference::Auto, &unsupported)
    );

    let invalid = CaptureError::TargetInvalid {
        detail: "bad hwnd".to_owned(),
    };
    assert!(!should_fallback_to_dxgi(
        CaptureBackendPreference::Auto,
        &invalid
    ));
}

#[test]
fn invalid_hwnd_surfaces_capture_target_invalid() {
    let config = CaptureConfig {
        target: CaptureTarget::Window { hwnd: 0 },
        ..CaptureConfig::default()
    };
    println!("before invalid_hwnd target={:?}", config.target);

    let err = resolve_capture_target(&config)
        .err()
        .unwrap_or_else(|| panic!("invalid hwnd should fail"));
    println!("after invalid_hwnd error_code={}", err.code());
    assert_eq!(err.code(), error_codes::CAPTURE_TARGET_INVALID);
}

#[test]
fn dxgi_backend_rejects_window_targets_before_thread_spawn() {
    let config = CaptureConfig {
        target: CaptureTarget::Window { hwnd: 1 },
        backend_preference: CaptureBackendPreference::DxgiDuplication,
        ..CaptureConfig::default()
    };
    println!(
        "before dxgi_window target={:?} selected_backend={:?}",
        config.target,
        config.selected_backend()
    );

    let err = resolve_capture_target(&config)
        .err()
        .unwrap_or_else(|| panic!("DXGI window target should fail"));
    println!("after dxgi_window error_code={} error={err}", err.code());
    assert_eq!(err.code(), error_codes::CAPTURE_TARGET_INVALID);
    assert!(err.to_string().contains("monitor targets only"));
}

#[test]
fn target_lost_error_surfaces_code() {
    let err = CaptureError::TargetLost {
        detail: "synthetic target loss".to_owned(),
    };
    println!("target_lost error_code={}", err.code());
    assert_eq!(err.code(), error_codes::CAPTURE_TARGET_LOST);
}

// Real capture only exists on Windows; without the removed synthetic mock there
// are no frames to drive this on other platforms, so it is Windows-only.
#[cfg(windows)]
#[test]
fn capture_channel_capacity_is_exactly_two_and_drops_oldest() -> Result<(), CaptureError> {
    let _guard = CAPTURE_LOCK
        .lock()
        .unwrap_or_else(|err| panic!("capture lock poisoned: {err}"));
    let handle = spawn_capture_loop(CaptureConfig {
        min_update_interval_ms: 1,
        dirty_region_only: false,
        ..CaptureConfig::default()
    })?;
    let stats = handle.stats();
    println!(
        "before slow_consumer captured={} dropped={} channel_len={}",
        stats.frames_captured(),
        stats.frames_dropped(),
        handle.receiver().len()
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while std::time::Instant::now() < deadline
        && (stats.frames_captured() <= 2 || stats.frames_dropped() == 0)
    {
        thread::sleep(Duration::from_millis(10));
    }

    println!(
        "after slow_consumer captured={} dropped={} channel_len={}",
        stats.frames_captured(),
        stats.frames_dropped(),
        handle.receiver().len()
    );
    assert!(stats.frames_captured() > 2);
    assert!(stats.frames_dropped() > 0);
    assert_eq!(CAPTURE_CHANNEL_CAPACITY, 2);
    assert!(handle.receiver().len() <= CAPTURE_CHANNEL_CAPACITY);
    handle.stop()
}

// Windows-only: the capture thread now fails loudly off Windows (no real
// backend), so `stop()` would surface that error. Non-Windows priority is
// covered by `dpi_awareness_is_noop_off_windows`.
#[cfg(windows)]
#[test]
fn capture_thread_priority_is_recorded() -> Result<(), CaptureError> {
    let _guard = CAPTURE_LOCK
        .lock()
        .unwrap_or_else(|err| panic!("capture lock poisoned: {err}"));
    let handle = spawn_capture_loop(CaptureConfig {
        min_update_interval_ms: 1,
        ..CaptureConfig::default()
    })?;
    let stats = handle.stats();
    println!("before priority_readback={:?}", stats.thread_priority());
    thread::sleep(Duration::from_millis(20));
    let priority = stats.thread_priority();
    println!("after priority_readback={priority:?}");
    if cfg!(windows) {
        assert_eq!(priority, CaptureThreadPriority::TimeCritical);
    } else {
        assert_eq!(priority, CaptureThreadPriority::Unsupported);
    }
    handle.stop()
}

#[test]
fn coordinate_transform_manual_edge_cases_round_trip() {
    let cases = [
        (Point { x: 0, y: 0 }, Point { x: 0, y: 0 }),
        (
            Point {
                x: 100_000,
                y: -100_000,
            },
            Point {
                x: -10_000,
                y: 10_000,
            },
        ),
        (
            Point {
                x: -100_000,
                y: 100_000,
            },
            Point {
                x: 10_000,
                y: -10_000,
            },
        ),
    ];

    for (point, origin) in cases {
        println!("before transform point={point:?} origin={origin:?}");
        let screen = window_to_screen_with_origin(point, origin);
        let round_trip = screen_to_window_with_origin(screen, origin);
        println!("after transform screen={screen:?} round_trip={round_trip:?}");
        assert_eq!(round_trip, point);
    }
}

// Windows-only: switching sessions joins capture threads, which now return the
// honest "unsupported" error off Windows instead of mock success.
#[cfg(windows)]
#[test]
fn switching_capture_target_stops_previous_session() -> Result<(), CaptureError> {
    let _guard = CAPTURE_LOCK
        .lock()
        .unwrap_or_else(|err| panic!("capture lock poisoned: {err}"));
    let mut controller = CaptureController::new();
    assert_eq!(controller.switch_to(CaptureConfig::default())?, 1);
    let first_stop = controller.active().map_or_else(
        || panic!("capture handle should be active"),
        |handle| handle.stop.clone(),
    );
    assert_eq!(
        controller.switch_to(CaptureConfig {
            target: CaptureTarget::Monitor { monitor_index: 0 },
            ..CaptureConfig::default()
        })?,
        2
    );
    assert!(first_stop.load(Ordering::Relaxed));
    Ok(())
}

proptest! {
    #[test]
    fn coordinate_transform_origin_round_trip(
        x in -100_000_i32..100_000,
        y in -100_000_i32..100_000,
        ox in -10_000_i32..10_000,
        oy in -10_000_i32..10_000,
    ) {
        let point = Point { x, y };
        let origin = Point { x: ox, y: oy };
        let screen = window_to_screen_with_origin(point, origin);
        prop_assert_eq!(screen_to_window_with_origin(screen, origin), point);
    }
}

#[test]
fn dpi_awareness_is_noop_off_windows() -> Result<(), CaptureError> {
    if cfg!(windows) {
        return Ok(());
    }

    assert_eq!(
        init_process_dpi_awareness()?,
        DpiAwarenessStatus::Unsupported
    );
    assert_eq!(
        current_thread_priority(),
        CaptureThreadPriority::Unsupported
    );
    Ok(())
}

#[test]
fn dpi_awareness_readback_matches_platform() -> Result<(), CaptureError> {
    let before = is_per_monitor_v2_dpi_aware();
    let status = init_process_dpi_awareness()?;
    let after = is_per_monitor_v2_dpi_aware();
    println!("dpi_readback before={before} status={status:?} after={after}");
    if cfg!(windows) {
        assert!(after);
    } else {
        assert_eq!(status, DpiAwarenessStatus::Unsupported);
        assert!(!after);
    }
    Ok(())
}
