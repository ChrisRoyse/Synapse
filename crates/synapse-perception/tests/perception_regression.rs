use std::{
    collections::BTreeMap,
    error::Error,
    io::{self, Write},
};

use chrono::Utc;
use synapse_core::{
    AccessibleNode, AudioContext, DetectedEntity, FocusedElement, ForegroundContext, FsEvent,
    FsEventKind, HudReadings, PerceptionMode, Rect, SensorStatus, UiaPattern, element_id,
    entity_id, error_codes,
};
use synapse_perception::{
    A11yTreeSummary, ObservationAssembler, ObservationInput, ObserveInclude, OcrProvider,
    TextRegion, auto_mode, auto_mode_with_a11y, parse_perception_mode, read_text,
    read_text_with_provider,
};

type TestResult = Result<(), Box<dyn Error>>;

fn regression_log(args: std::fmt::Arguments<'_>) -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    stdout.write_fmt(args)?;
    stdout.write_all(b"\n")
}

#[derive(Clone, Debug)]
struct StaticOcr {
    output: Vec<TextRegion>,
}

impl OcrProvider for StaticOcr {
    fn read_text(&self, _region: Rect) -> synapse_perception::PerceptionResult<Vec<TextRegion>> {
        Ok(self.output.clone())
    }
}

fn notepad_input() -> ObservationInput {
    let at = Utc::now();
    let focused_id = element_id(0x1234, "0000002a00001234");
    let elements = vec![
        node(0, "Notepad", "Window", false),
        node(1, "Document", "Edit", true),
        node(1, "File", "MenuItem", false),
        node(1, "Edit", "MenuItem", false),
        node(1, "View", "MenuItem", false),
        node(1, "Status", "Text", false),
    ];
    let mut latency = BTreeMap::new();
    latency.insert("a11y".to_owned(), 1.25);
    latency.insert("capture".to_owned(), 0.50);
    latency.insert("detection".to_owned(), 0.0);
    latency.insert("audio".to_owned(), 0.0);
    ObservationInput {
        foreground: ForegroundContext {
            hwnd: 0x1234,
            pid: 44,
            process_name: "notepad.exe".to_owned(),
            process_path: "C:\\Windows\\System32\\notepad.exe".to_owned(),
            window_title: "manual-regression.txt - Notepad".to_owned(),
            window_bounds: Rect {
                x: 10,
                y: 20,
                w: 800,
                h: 600,
            },
            monitor_index: 0,
            dpi_scale: 1.0,
            profile_id: None,
            steam_appid: None,
            is_fullscreen: false,
            is_dwm_composed: true,
        },
        is_minimized: false,
        focused: Some(FocusedElement {
            element_id: focused_id,
            name: "Document".to_owned(),
            role: "Edit".to_owned(),
            automation_id: Some("15".to_owned()),
            bbox: Rect {
                x: 12,
                y: 80,
                w: 760,
                h: 480,
            },
            enabled: true,
            patterns: vec![UiaPattern::Text, UiaPattern::Value],
            value: Some("synthetic notepad text".to_owned()),
            selected_text: None,
        }),
        elements,
        entities: vec![DetectedEntity {
            entity_id: entity_id(9),
            track_id: 9,
            class_label: "cursor".to_owned(),
            bbox: Rect {
                x: 40,
                y: 90,
                w: 8,
                h: 20,
            },
            confidence: 0.80,
            first_seen_at: at,
            last_seen_at: at,
            velocity_px_per_s: None,
        }],
        hud: HudReadings::default(),
        audio: AudioContext::default(),
        recent_events: Vec::new(),
        clipboard_summary: None,
        fs_recent: Vec::new(),
        sensor_latency_ms: latency,
        a11y_status: SensorStatus::Healthy,
        capture_status: SensorStatus::Healthy,
        detection_status: SensorStatus::Disabled,
        audio_status: SensorStatus::Disabled,
        mode_override: None,
        capture_config: None,
        capture_runtime: None,
        input_backends: None,
        cdp: None,
        web_path: None,
    }
}

fn node(depth: u32, name: &str, role: &str, focused: bool) -> AccessibleNode {
    let depth_i32 = i32::try_from(depth).unwrap_or(0);
    AccessibleNode {
        element_id: element_id(0x1234, &format!("0000002a{depth:08x}")),
        parent: (depth > 0).then(|| element_id(0x1234, "0000002a00000000")),
        name: name.to_owned(),
        role: role.to_owned(),
        automation_id: None,
        value: None,
        bbox: Rect {
            x: 10 + depth_i32,
            y: 20 + depth_i32,
            w: 100,
            h: 30,
        },
        enabled: true,
        focused,
        patterns: Vec::new(),
        children_count: 0,
        depth,
    }
}

#[test]
fn assemble_default_notepad_under_6kb_with_latency_readback() -> TestResult {
    let input = notepad_input();
    regression_log(format_args!(
        "regression_check=observation edge=notepad_default before=process:{} nodes:{}",
        input.foreground.process_name,
        input.elements.len()
    ))?;
    let observation = ObservationAssembler::new().assemble(ObserveInclude::default(), input)?;
    let bytes = serde_json::to_vec(&observation)?;
    regression_log(format_args!(
        "regression_check=observation edge=notepad_default after=bytes:{} mode:{:?} process:{} focused_role:{} a11y_latency:{:?}",
        bytes.len(),
        observation.mode,
        observation.foreground.process_name,
        observation
            .focused
            .as_ref()
            .map_or("", |item| item.role.as_str()),
        observation.diagnostics.sensor_latency_ms.get("a11y")
    ))?;
    assert!(bytes.len() <= 6 * 1024);
    assert_eq!(observation.mode, PerceptionMode::A11yOnly);
    assert_eq!(observation.foreground.process_name, "notepad.exe");
    assert_eq!(
        observation.focused.as_ref().map(|item| item.role.as_str()),
        Some("Edit")
    );
    assert!(
        observation
            .diagnostics
            .sensor_latency_ms
            .contains_key("capture")
    );
    assert_eq!(
        observation.diagnostics.size_bytes,
        u32::try_from(bytes.len())?
    );
    Ok(())
}

#[test]
fn include_flags_are_independently_testable() -> TestResult {
    let assembler = ObservationAssembler::new();
    let no_subtree = assembler.assemble(
        ObserveInclude {
            elements: false,
            ..ObserveInclude::default()
        },
        notepad_input(),
    )?;
    regression_log(format_args!(
        "regression_check=observe_include edge=no_subtree after=focused:{} elements:{} truncated:{}",
        no_subtree.focused.is_some(),
        no_subtree.elements.len(),
        no_subtree.diagnostics.elements_truncated
    ))?;
    assert!(no_subtree.focused.is_some());
    assert!(no_subtree.elements.is_empty());
    assert!(no_subtree.diagnostics.elements_truncated);

    let no_focus = assembler.assemble(
        ObserveInclude {
            focused: false,
            ..ObserveInclude::default()
        },
        notepad_input(),
    )?;
    regression_log(format_args!(
        "regression_check=observe_include edge=no_focused after=focused:{} elements:{}",
        no_focus.focused.is_some(),
        no_focus.elements.len()
    ))?;
    assert!(no_focus.focused.is_none());
    assert!(!no_focus.elements.is_empty());

    let limited_detection = assembler.assemble(
        ObserveInclude {
            entities: true,
            max_entities: 0,
            ..ObserveInclude::default()
        },
        notepad_input(),
    )?;
    regression_log(format_args!(
        "regression_check=observe_include edge=detections_limited after=entities:{} truncated:{}",
        limited_detection.entities.len(),
        limited_detection.diagnostics.entities_truncated
    ))?;
    assert!(limited_detection.entities.is_empty());
    assert!(limited_detection.diagnostics.entities_truncated);
    Ok(())
}

#[test]
fn element_pagination_exposes_next_offset_until_last_page() -> TestResult {
    let mut input = notepad_input();
    input.elements = (0..7)
        .map(|index| AccessibleNode {
            element_id: element_id(0x1234, &format!("0000002a{index:08x}")),
            parent: None,
            name: format!("Node {index}"),
            role: "Text".to_owned(),
            automation_id: None,
            value: None,
            bbox: Rect {
                x: index,
                y: 20,
                w: 100,
                h: 30,
            },
            enabled: true,
            focused: false,
            patterns: Vec::new(),
            children_count: 0,
            depth: 1,
        })
        .collect();

    let first = ObservationAssembler::new().assemble(
        ObserveInclude {
            max_subtree_nodes: 3,
            ..ObserveInclude::default()
        },
        input.clone(),
    )?;
    regression_log(format_args!(
        "regression_check=observe_pagination edge=first after=names:{:?} page:{:?}",
        first
            .elements
            .iter()
            .map(|node| node.name.as_str())
            .collect::<Vec<_>>(),
        first.diagnostics.elements_page
    ))?;
    assert!(first.diagnostics.elements_truncated);
    assert_eq!(first.elements.len(), 3);
    assert_eq!(
        first
            .diagnostics
            .elements_page
            .as_ref()
            .and_then(|page| page.next_offset),
        Some(3)
    );

    let last = ObservationAssembler::new().assemble(
        ObserveInclude {
            max_subtree_nodes: 3,
            element_offset: 6,
            ..ObserveInclude::default()
        },
        input,
    )?;
    regression_log(format_args!(
        "regression_check=observe_pagination edge=last after=names:{:?} page:{:?}",
        last.elements
            .iter()
            .map(|node| node.name.as_str())
            .collect::<Vec<_>>(),
        last.diagnostics.elements_page
    ))?;
    assert!(!last.diagnostics.elements_truncated);
    assert_eq!(last.elements.len(), 1);
    assert_eq!(last.elements[0].name, "Node 6");
    let page = last
        .diagnostics
        .elements_page
        .as_ref()
        .expect("element page metadata should be present");
    assert_eq!(page.total, 7);
    assert_eq!(page.offset, 6);
    assert_eq!(page.limit, 3);
    assert_eq!(page.next_offset, None);
    Ok(())
}

#[test]
fn assembler_all_sensors_unavailable_fails_closed() -> TestResult {
    let input = ObservationInput::new(notepad_input().foreground);
    regression_log(format_args!(
        "regression_check=observation edge=no_sensors before=a11y:{:?} capture:{:?} detection:{:?} audio:{:?}",
        input.a11y_status, input.capture_status, input.detection_status, input.audio_status
    ))?;
    let after = ObservationAssembler::new().assemble(ObserveInclude::default(), input);
    regression_log(format_args!(
        "regression_check=observation edge=no_sensors after={after:?}"
    ))?;
    assert_eq!(
        after.err().map(|err| err.code()),
        Some(error_codes::OBSERVE_NO_PERCEPTION_AVAILABLE)
    );
    Ok(())
}

/// #1508: a filesystem-only observe reads host-wide state and must succeed even
/// when every window/screen/audio sensor is unavailable (the exact state a
/// no-window global-only readback produces). It must not fail closed the way a
/// window-perception observe does, and it must carry the requested fs data.
#[test]
fn assembler_global_only_fs_succeeds_without_window_sensors() -> TestResult {
    let fs_only = ObserveInclude {
        focused: false,
        elements: false,
        entities: false,
        hud: false,
        audio: false,
        events: false,
        clipboard: false,
        fs: true,
        diagnostics: false,
        ..ObserveInclude::default()
    };
    assert!(
        !fs_only.requires_window_perception(),
        "fs-only observe must not require window perception"
    );

    // No window was observed: empty foreground, every sensor Unavailable/Disabled.
    let mut input = ObservationInput::new(ForegroundContext {
        hwnd: 0,
        pid: 0,
        process_name: String::new(),
        process_path: String::new(),
        window_title: String::new(),
        window_bounds: Rect {
            x: 0,
            y: 0,
            w: 0,
            h: 0,
        },
        monitor_index: 0,
        dpi_scale: 1.0,
        profile_id: None,
        steam_appid: None,
        is_fullscreen: false,
        is_dwm_composed: false,
    });
    input.fs_recent = vec![FsEvent {
        at: Utc::now(),
        path: "C:\\code\\GameEditor\\target\\regression\\readback.json".to_owned(),
        kind: FsEventKind::Modified,
        size_bytes: Some(2048),
    }];

    regression_log(format_args!(
        "regression_check=observation edge=global_only_fs before=a11y:{:?} capture:{:?} fs_events:{}",
        input.a11y_status,
        input.capture_status,
        input.fs_recent.len()
    ))?;
    let observation = ObservationAssembler::new().assemble(fs_only, input)?;
    regression_log(format_args!(
        "regression_check=observation edge=global_only_fs after=fs_recent:{} focused:{} elements:{}",
        observation.fs_recent.len(),
        observation.focused.is_some(),
        observation.elements.len()
    ))?;
    assert_eq!(observation.fs_recent.len(), 1);
    assert_eq!(
        observation.fs_recent[0].path,
        "C:\\code\\GameEditor\\target\\regression\\readback.json"
    );
    assert!(observation.focused.is_none());
    assert!(observation.elements.is_empty());
    Ok(())
}

#[test]
fn auto_mode_edges_and_invalid_manual_override() -> TestResult {
    let notepad = notepad_input().foreground;
    let notepad_default = auto_mode(&notepad);
    regression_log(format_args!(
        "regression_check=perception_mode edge=notepad_default before=process:{} after={notepad_default:?}",
        notepad.process_name
    ))?;
    assert_eq!(notepad_default, PerceptionMode::A11yOnly);

    let notepad_mode = auto_mode_with_a11y(
        &notepad,
        &A11yTreeSummary {
            node_count: 6,
            max_depth: 1,
        },
    );
    regression_log(format_args!(
        "regression_check=perception_mode edge=notepad_rich_a11y after={notepad_mode:?}"
    ))?;
    assert_eq!(notepad_mode, PerceptionMode::A11yOnly);

    let mut game = notepad.clone();
    game.process_name = "starfield.exe".to_owned();
    let game_default = auto_mode(&game);
    regression_log(format_args!(
        "regression_check=perception_mode edge=game_default before=process:{} after={game_default:?}",
        game.process_name
    ))?;
    assert_eq!(game_default, PerceptionMode::Hybrid);

    let game_mode = auto_mode_with_a11y(
        &game,
        &A11yTreeSummary {
            node_count: 20,
            max_depth: 3,
        },
    );
    regression_log(format_args!(
        "regression_check=perception_mode edge=game_rich_a11y after={game_mode:?}"
    ))?;
    assert_eq!(game_mode, PerceptionMode::Hybrid);

    let sparse_mode = auto_mode_with_a11y(
        &notepad,
        &A11yTreeSummary {
            node_count: 1,
            max_depth: 0,
        },
    );
    regression_log(format_args!(
        "regression_check=perception_mode edge=sparse_a11y after={sparse_mode:?}"
    ))?;
    assert_eq!(sparse_mode, PerceptionMode::Hybrid);

    let invalid = parse_perception_mode("telepathy");
    regression_log(format_args!(
        "regression_check=perception_mode edge=invalid before=telepathy after={invalid:?}"
    ))?;
    assert_eq!(
        invalid.err().map(|err| err.code()),
        Some(error_codes::PERCEPTION_MODE_INVALID)
    );
    Ok(())
}

#[test]
fn ocr_empty_region_and_backend_edges_are_observable() -> TestResult {
    let empty = Rect {
        x: 0,
        y: 0,
        w: 0,
        h: 64,
    };
    regression_log(format_args!(
        "regression_check=ocr edge=empty_region before=region:{empty:?}"
    ))?;
    let empty_after = read_text(empty);
    regression_log(format_args!(
        "regression_check=ocr edge=empty_region after={empty_after:?}"
    ))?;
    assert_eq!(
        empty_after.err().map(|err| err.code()),
        Some(error_codes::OCR_NO_TEXT)
    );

    #[cfg(not(windows))]
    {
        let region = Rect {
            x: 0,
            y: 0,
            w: 256,
            h: 64,
        };
        regression_log(format_args!(
            "regression_check=ocr edge=backend before=region:{region:?}"
        ))?;
        let backend_after = read_text(region);
        regression_log(format_args!(
            "regression_check=ocr edge=backend after={backend_after:?}"
        ))?;
        match backend_after {
            Ok(words) => {
                assert!(
                    !words.is_empty(),
                    "available platform OCR must return at least one word"
                );
                regression_log(format_args!(
                    "regression_check=ocr edge=backend live_words={}",
                    words.len()
                ))?;
            }
            Err(err) => {
                assert!(
                    matches!(
                        err.code(),
                        error_codes::OCR_BACKEND_UNAVAILABLE | error_codes::OCR_NO_TEXT
                    ),
                    "platform OCR must fail closed with stable OCR code, got {err:?}"
                );
            }
        }
    }
    Ok(())
}

#[test]
fn ocr_provider_happy_path_and_empty_result_are_observable() -> TestResult {
    let region = Rect {
        x: 5,
        y: 7,
        w: 256,
        h: 64,
    };
    let provider = StaticOcr {
        output: vec![TextRegion {
            text: "Synapse".to_owned(),
            bbox: Rect {
                x: 9,
                y: 11,
                w: 72,
                h: 18,
            },
            confidence: 0.99,
            confidence_source: synapse_perception::TextRegionConfidenceSource::Engine,
        }],
    };
    regression_log(format_args!(
        "regression_check=ocr edge=synthetic_text before=region:{region:?} provider_words:{}",
        provider.output.len()
    ))?;
    let words = read_text_with_provider(&provider, region)?;
    let first = words
        .first()
        .ok_or_else(|| io::Error::other("missing OCR first word"))?;
    regression_log(format_args!(
        "regression_check=ocr edge=synthetic_text after=count:{} first:{} bbox:{:?}",
        words.len(),
        first.text,
        first.bbox
    ))?;
    assert_eq!(words.len(), 1);
    assert_eq!(first.text, "Synapse");
    assert_eq!(
        first.bbox,
        Rect {
            x: 9,
            y: 11,
            w: 72,
            h: 18
        }
    );

    let empty_provider = StaticOcr { output: Vec::new() };
    regression_log(format_args!(
        "regression_check=ocr edge=synthetic_empty before=region:{region:?} provider_words:{}",
        empty_provider.output.len()
    ))?;
    let empty = read_text_with_provider(&empty_provider, region);
    regression_log(format_args!(
        "regression_check=ocr edge=synthetic_empty after={empty:?}"
    ))?;
    assert_eq!(
        empty.err().map(|err| err.code()),
        Some(error_codes::OCR_NO_TEXT)
    );
    Ok(())
}

#[cfg(windows)]
#[test]
fn winrt_blank_bitmap_returns_no_text_or_backend_unavailable() -> TestResult {
    use synapse_perception::read_text_from_software_bitmap;
    use windows::Graphics::Imaging::{BitmapPixelFormat, SoftwareBitmap};

    let region = Rect {
        x: 0,
        y: 0,
        w: 256,
        h: 64,
    };
    regression_log(format_args!(
        "regression_check=ocr edge=winrt_blank before=region:{region:?} bitmap=256x64"
    ))?;
    let Ok(bitmap) = SoftwareBitmap::Create(BitmapPixelFormat::Bgra8, region.w, region.h) else {
        regression_log(format_args!(
            "regression_check=ocr edge=winrt_blank after=backend_unavailable:software_bitmap_activation"
        ))?;
        return Ok(());
    };
    let after = read_text_from_software_bitmap(region, &bitmap);
    regression_log(format_args!(
        "regression_check=ocr edge=winrt_blank after={after:?}"
    ))?;
    let code = after.err().map(|err| err.code());
    assert!(matches!(
        code,
        Some(error_codes::OCR_NO_TEXT | error_codes::OCR_BACKEND_UNAVAILABLE)
    ));
    Ok(())
}

#[cfg(windows)]
#[test]
#[ignore = "requires an interactive Windows desktop with WinRT OCR"]
fn winrt_text_window_region_read_text_native() -> TestResult {
    use std::{
        process::Command,
        time::{SystemTime, UNIX_EPOCH},
    };

    let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis();
    let title = format!("synapse-ocr-regression-{stamp}");
    let expected = "Synapse OCR Regression 2468";
    let script = format!(
        "Add-Type -AssemblyName System.Windows.Forms; \
         Add-Type -AssemblyName System.Drawing; \
         $form = New-Object System.Windows.Forms.Form; \
         $form.Text = '{title}'; \
         $form.Width = 900; $form.Height = 260; \
         $form.StartPosition = 'CenterScreen'; $form.TopMost = $true; \
         $label = New-Object System.Windows.Forms.Label; \
         $label.Text = '{expected}'; \
         $label.Font = New-Object System.Drawing.Font('Segoe UI', 36); \
         $label.Dock = 'Fill'; $label.TextAlign = 'MiddleCenter'; \
         $form.Controls.Add($label); \
         $form.Add_Shown({{ $form.Activate() }}); \
         [void]$form.ShowDialog();"
    );
    regression_log(format_args!(
        "regression_check=ocr edge=winrt_text_window before=title:{title} text={expected}"
    ))?;
    let mut child = Command::new("powershell.exe")
        .args(["-NoProfile", "-STA", "-Command", &script])
        .spawn()?;

    let result = (|| -> TestResult {
        let hwnd = wait_for_window_hwnd(child.id())?;
        regression_log(format_args!(
            "regression_check=ocr edge=winrt_text_window hwnd_readback={hwnd}"
        ))?;
        let tree = synapse_a11y::snapshot_window_from_hwnd(hwnd, 2)?;
        let target = tree
            .nodes
            .iter()
            .find(|node| node.name.contains("Synapse") && node.bbox.w > 0 && node.bbox.h > 0)
            .or_else(|| {
                tree.nodes
                    .iter()
                    .find(|node| node.focused && node.bbox.w > 0 && node.bbox.h > 0)
            })
            .or_else(|| {
                tree.nodes
                    .iter()
                    .find(|node| node.bbox.w > 0 && node.bbox.h > 0)
            })
            .ok_or_else(|| io::Error::other("no visible text-window node"))?;
        regression_log(format_args!(
            "regression_check=ocr edge=winrt_text_window focus_readback=root:{} nodes:{} role:{} name:{} bbox:{:?}",
            tree.root,
            tree.nodes.len(),
            target.role,
            target.name,
            target.bbox
        ))?;
        let words = read_text(target.bbox)?;
        let text = words
            .iter()
            .map(|word| word.text.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        regression_log(format_args!(
            "regression_check=ocr edge=winrt_text_window after=text:{text} words:{}",
            words.len()
        ))?;
        assert!(text.contains("Synapse"), "actual OCR text: {text}");
        Ok(())
    })();

    let _ = child.kill();
    let _ = child.wait();
    result
}

#[cfg(windows)]
#[test]
#[ignore = "requires an interactive Windows desktop with WinRT OCR"]
fn winrt_ocr_capture_vs_recognize_timing_native() -> TestResult {
    use std::time::Instant;

    use synapse_capture::screen_region_to_software_bitmap;
    use synapse_perception::read_text_from_software_bitmap;

    for (name, region) in [
        (
            "small_256x64",
            Rect {
                x: 0,
                y: 0,
                w: 256,
                h: 64,
            },
        ),
        (
            "full_1080p",
            Rect {
                x: 0,
                y: 0,
                w: 1920,
                h: 1080,
            },
        ),
    ] {
        let mut capture_ms = Vec::new();
        let mut recognize_ms = Vec::new();
        regression_log(format_args!(
            "regression_check=ocr_timing edge={name} before=region:{region:?} samples:10"
        ))?;
        for _ in 0..10 {
            let capture_started = Instant::now();
            let captured = screen_region_to_software_bitmap(region)?;
            capture_ms.push(capture_started.elapsed().as_secs_f64() * 1_000.0);

            let recognize_started = Instant::now();
            let result = read_text_from_software_bitmap(captured.region, &captured.bitmap);
            recognize_ms.push(recognize_started.elapsed().as_secs_f64() * 1_000.0);
            if matches!(
                result
                    .as_ref()
                    .err()
                    .map(synapse_perception::PerceptionError::code),
                Some(error_codes::OCR_BACKEND_UNAVAILABLE)
            ) {
                return Err(io::Error::other("WinRT OCR backend unavailable").into());
            }
        }
        regression_log(format_args!(
            "regression_check=ocr_timing edge={name} after=capture_p99_ms:{:.3} recognize_p99_ms:{:.3}",
            p99_ms(capture_ms),
            p99_ms(recognize_ms)
        ))?;
    }
    Ok(())
}

#[cfg(windows)]
fn wait_for_window_hwnd(pid: u32) -> Result<i64, Box<dyn Error>> {
    for _ in 0..20 {
        match synapse_a11y::snapshot_window_for_process(pid, 0) {
            Ok(tree) => {
                return Ok(tree.root.parts()?.hwnd);
            }
            Err(_) => {
                std::thread::sleep(std::time::Duration::from_millis(250));
            }
        }
    }
    Err(io::Error::other(format!("window for process id {pid} was not visible")).into())
}

#[cfg(windows)]
fn p99_ms(mut samples: Vec<f64>) -> f64 {
    samples.sort_by(f64::total_cmp);
    samples.last().copied().unwrap_or_default()
}

/// #882: a web-form-shaped element list (structural noise + deep interactable
/// fields) filtered with `interactable_only` keeps only actionable controls,
/// regardless of depth, and pages over the filtered set.
#[test]
fn interactable_only_keeps_form_controls_and_ignores_depth() -> TestResult {
    let mut input = notepad_input();
    // Mixed vocabulary: web AX roles (empty patterns) + UIA roles (patterns).
    input.elements = vec![
        web_node(0, "RootWebArea", "Page"),
        web_node(3, "generic", ""),
        web_node(4, "heading", "Upload video"),
        web_node(9, "textbox", "Title"),
        web_node(11, "textbox", "Description"),
        web_node(5, "button", "Next"),
        web_node(6, "link", "Help"),
        web_node(7, "StaticText", "Details"),
        web_node(8, "image", "Thumbnail"),
        uia_node(2, "Edit", "Search", vec![UiaPattern::Value]),
        uia_node(1, "Pane", "Sidebar", vec![UiaPattern::Scroll]),
        uia_node(2, "check box", "Agree", Vec::new()),
        disabled_node(4, "button", "Submit"),
    ];
    let before_count = input.elements.len();
    regression_log(format_args!(
        "regression_check=interactable edge=web_form before=nodes:{before_count}"
    ))?;

    let observation = ObservationAssembler::new().assemble(
        ObserveInclude {
            interactable_only: true,
            max_subtree_depth: 2,
            ..ObserveInclude::default()
        },
        input,
    )?;
    let roles: Vec<(String, String, u32)> = observation
        .elements
        .iter()
        .map(|node| (node.role.clone(), node.name.clone(), node.depth))
        .collect();
    regression_log(format_args!(
        "regression_check=interactable edge=web_form after=kept:{} roles:{roles:?}",
        observation.elements.len()
    ))?;

    // Both deep textboxes survive (depth 9 and 11 > max_subtree_depth 2).
    assert!(
        roles
            .iter()
            .any(|(role, name, depth)| role == "textbox" && name == "Title" && *depth == 9)
    );
    assert!(roles.iter().any(|(_, name, _)| name == "Description"));
    // Buttons, links, UIA edit, and the localized "check box" role survive.
    assert!(
        roles
            .iter()
            .any(|(role, name, _)| role == "button" && name == "Next")
    );
    assert!(roles.iter().any(|(role, _, _)| role == "link"));
    assert!(roles.iter().any(|(role, _, _)| role == "Edit"));
    assert!(roles.iter().any(|(role, _, _)| role == "check box"));
    // Structural/decorative/disabled nodes are gone.
    assert!(!roles.iter().any(|(role, _, _)| {
        matches!(
            role.as_str(),
            "RootWebArea" | "generic" | "heading" | "StaticText" | "image" | "Pane"
        )
    }));
    assert!(!roles.iter().any(|(_, name, _)| name == "Submit"));
    assert_eq!(observation.elements.len(), 6);
    Ok(())
}

/// #882: diagnostics payloads (`input_backends`, cdp evidence, capture blocks)
/// are emitted only when the include set requests diagnostics; `web_path` always
/// survives as the fidelity signal.
#[test]
fn diagnostics_blocks_are_suppressed_when_not_requested() -> TestResult {
    let suppressed = ObservationAssembler::new().assemble(
        ObserveInclude {
            diagnostics: false,
            ..ObserveInclude::default()
        },
        diagnostics_heavy_input(),
    )?;
    regression_log(format_args!(
        "regression_check=diagnostics edge=suppressed after=input_backends:{} cdp:{} capture_config:{} capture_runtime:{} web_path:{:?} bytes:{}",
        suppressed.diagnostics.input_backends.is_some(),
        suppressed.diagnostics.cdp.is_some(),
        suppressed.diagnostics.capture_config.is_some(),
        suppressed.diagnostics.capture_runtime.is_some(),
        suppressed.diagnostics.web_path,
        suppressed.diagnostics.size_bytes
    ))?;
    assert!(suppressed.diagnostics.input_backends.is_none());
    assert!(suppressed.diagnostics.cdp.is_none());
    assert!(suppressed.diagnostics.capture_config.is_none());
    assert!(suppressed.diagnostics.capture_runtime.is_none());
    assert_eq!(
        suppressed.diagnostics.web_path,
        Some(synapse_core::WebPerceptionPath::Cdp)
    );

    let included = ObservationAssembler::new()
        .assemble(ObserveInclude::default(), diagnostics_heavy_input())?;
    regression_log(format_args!(
        "regression_check=diagnostics edge=default_included after=input_backends:{} cdp:{} bytes:{}",
        included.diagnostics.input_backends.is_some(),
        included.diagnostics.cdp.is_some(),
        included.diagnostics.size_bytes
    ))?;
    assert!(included.diagnostics.input_backends.is_some());
    assert!(included.diagnostics.cdp.is_some());
    assert!(included.diagnostics.size_bytes > suppressed.diagnostics.size_bytes);
    Ok(())
}

fn diagnostics_heavy_input() -> ObservationInput {
    let mut input = notepad_input();
    input.cdp = Some(synapse_core::CdpDiagnostics::unreachable_with_probe(
        "chrome.exe",
        "A11Y_CDP_UNREACHABLE",
        vec![9222, 9223, 9224, 9225],
        "no debug port reachable on loopback candidates",
    ));
    input.web_path = Some(synapse_core::WebPerceptionPath::Cdp);
    input.input_backends = Some(synapse_core::InputBackendDiagnostics {
        source: "test".to_owned(),
        mouse_default: "software".to_owned(),
        keyboard_default: "software".to_owned(),
        pad_default: "vigem".to_owned(),
        release_all_default: "software".to_owned(),
        mouse: vec![
            backend_capability("software"),
            backend_capability("hardware"),
        ],
        keyboard: vec![
            backend_capability("software"),
            backend_capability("hardware"),
        ],
        pad: vec![backend_capability("vigem")],
        release_all: vec![backend_capability("software")],
    });
    input
}

fn backend_capability(backend: &str) -> synapse_core::InputBackendCapability {
    synapse_core::InputBackendCapability {
        backend: backend.to_owned(),
        available: true,
        reason_code: None,
        reason: None,
        host_boundary: false,
        transient: false,
    }
}

fn web_node(depth: u32, role: &str, name: &str) -> AccessibleNode {
    let mut item = node(depth, name, role, false);
    item.patterns = Vec::new();
    item
}

fn uia_node(depth: u32, role: &str, name: &str, patterns: Vec<UiaPattern>) -> AccessibleNode {
    let mut item = node(depth, name, role, false);
    item.patterns = patterns;
    item
}

fn disabled_node(depth: u32, role: &str, name: &str) -> AccessibleNode {
    let mut item = node(depth, name, role, false);
    item.enabled = false;
    item
}
