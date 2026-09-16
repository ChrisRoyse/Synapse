use std::{fs, time::UNIX_EPOCH};

use synapse_core::{
    Backend, HudExtractor, HudParser, HudRegion, PROFILE_SCHEMA_VERSION, ProfileUseScope,
    WindowEdge, default_hud_confidence_threshold, error_codes,
};
use synapse_profiles::{
    ProfileError, ScreenBounds, bundled_profiles_dir, parse_profile_bytes, parse_profile_file,
};

fn assert_f32_close(actual: f32, expected: f32) {
    assert!(
        (actual - expected).abs() <= f32::EPSILON,
        "actual={actual} expected={expected}"
    );
}

#[test]
fn bundled_profiles_parse_and_keep_natural_defaults() -> Result<(), Box<dyn std::error::Error>> {
    let dir = bundled_profiles_dir();
    let mut ids = Vec::new();
    for entry in fs::read_dir(&dir)? {
        let path = entry?.path();
        if path.extension().and_then(|value| value.to_str()) != Some("toml") {
            continue;
        }
        let loaded = parse_profile_file(&path)?;
        assert_eq!(
            loaded.schema_version,
            PROFILE_SCHEMA_VERSION,
            "{}",
            path.display()
        );
        assert_eq!(
            loaded.defaults.mouse_velocity_profile_default,
            "natural",
            "{}",
            path.display()
        );
        assert_eq!(
            loaded.defaults.keyboard_dynamics_default,
            "natural",
            "{}",
            path.display()
        );
        ids.push(loaded.profile.id);
    }
    ids.sort();
    assert_eq!(
        ids,
        [
            "acrobat",
            "calculator",
            "chrome",
            "cmd",
            "excel",
            "explorer",
            "firefox",
            "iexplore",
            "luanti.minetest",
            "minecraft.java",
            "mstsc",
            "notepad",
            "onenote",
            "outlook",
            "paint",
            "photos",
            "powerpoint",
            "powershell",
            "settings",
            "slack",
            "snippingtool",
            "taskmanager",
            "teams",
            "terminal",
            "vscode",
            "word",
            "wordpad",
            "zoom",
        ]
    );
    Ok(())
}

#[test]
fn bundled_minecraft_profile_carries_first_game_contract() -> Result<(), Box<dyn std::error::Error>>
{
    let path = bundled_profiles_dir().join("minecraft.java.toml");
    let loaded = parse_profile_file(&path)?;
    let profile = loaded.profile;

    assert_eq!(profile.id, "minecraft.java");
    assert_eq!(profile.use_scope, ProfileUseScope::SinglePlayer);
    assert_eq!(profile.matches.len(), 2);
    assert_eq!(
        profile.detection.model_id.as_deref(),
        Some("rtdetr_v2_s_coco_onnx")
    );
    assert_eq!(
        profile.detection.classes_of_interest,
        ["player", "zombie", "skeleton", "creeper", "villager"]
    );
    assert_eq!(profile.hud.len(), 3);
    let xp = profile
        .hud
        .iter()
        .find(|field| field.name == "minecraft.xp_level")
        .ok_or("minecraft.xp_level HUD field missing")?;
    assert!(matches!(xp.extractor, HudExtractor::WinrtOcr));
    assert!(matches!(
        xp.parser,
        HudParser::BoundedInteger {
            min: 0,
            max: 30,
            default_on_no_text: Some(0),
        }
    ));
    assert!(matches!(
        xp.region,
        HudRegion::AnchoredToEdge {
            edge: WindowEdge::BottomCenter,
            x_offset: -32,
            y_offset: -56,
            w: 64,
            h: 16,
        }
    ));
    assert_eq!(profile.keymap["attack"], "lmb");
    assert_eq!(profile.keymap["place"], "rmb");
    assert_eq!(profile.keymap["sneak"], "lshift");
    assert!(
        profile
            .event_extensions
            .iter()
            .any(|extension| extension.name == "creeper_nearby")
    );
    assert_eq!(
        profile.metadata["supported_use.remote_server_allowed"],
        "false"
    );
    assert_eq!(
        profile.metadata["runtime.minecraft.configured_host_status"],
        "launcher_installed_sign_in_required_java_runtime_not_verified"
    );
    Ok(())
}

#[test]
fn parser_rejects_synthetic_invalid_profiles_with_exact_codes() {
    let cases = [
        (
            "missing_matches.toml",
            r#"
id = "missing_matches"
label = "Missing Matches"
schema_version = 2
use_scope = "productivity"
mouse_velocity_profile_default = "natural"
keyboard_dynamics_default = "natural"
"#,
            error_codes::PROFILE_PARSE_ERROR,
        ),
        (
            "bad_regex.toml",
            r#"
id = "bad_regex"
label = "Bad Regex"
schema_version = 2
use_scope = "productivity"
mouse_velocity_profile_default = "natural"
keyboard_dynamics_default = "natural"

[[matches]]
title_regex = "("
"#,
            error_codes::PROFILE_PARSE_ERROR,
        ),
        (
            "future_schema.toml",
            r#"
id = "future_schema"
label = "Future Schema"
schema_version = 999
use_scope = "productivity"
mouse_velocity_profile_default = "natural"
keyboard_dynamics_default = "natural"

[[matches]]
exe = "future.exe"
"#,
            error_codes::PROFILE_VERSION_INCOMPATIBLE,
        ),
        (
            "invalid_keymap.toml",
            r#"
id = "invalid_keymap"
label = "Invalid Keymap"
schema_version = 2
use_scope = "productivity"
mouse_velocity_profile_default = "natural"
keyboard_dynamics_default = "natural"

[[matches]]
exe = "keys.exe"

[keymap]
bad = "ctrl+dragon"
"#,
            error_codes::PROFILE_KEYMAP_INVALID,
        ),
        (
            "instant_default.toml",
            r#"
id = "instant_default"
label = "Instant Default"
schema_version = 2
use_scope = "productivity"
mouse_velocity_profile_default = "instant"
keyboard_dynamics_default = "natural"

[[matches]]
exe = "instant.exe"
"#,
            error_codes::PROFILE_PARSE_ERROR,
        ),
    ];

    for (path, toml, code) in cases {
        let result = parse_profile_bytes(
            path,
            toml.as_bytes(),
            UNIX_EPOCH,
            ScreenBounds {
                width: 1920,
                height: 1080,
            },
        );
        let Err(error) = result else {
            panic!("{path} parsed successfully but should have failed");
        };
        assert_eq!(error.code(), code, "{path}: {error}");
    }
}

#[test]
fn parser_accepts_mouse_velocity_profile_default_and_legacy_curve_alias() {
    let new_key = r#"
id = "new_velocity_key"
label = "New Velocity Key"
schema_version = 2
use_scope = "productivity"
mouse_velocity_profile_default = "natural"
keyboard_dynamics_default = "natural"

[[matches]]
exe = "new_velocity_key.exe"
"#;
    let loaded = parse_profile_bytes(
        "new_velocity_key.toml",
        new_key.as_bytes(),
        UNIX_EPOCH,
        ScreenBounds {
            width: 1920,
            height: 1080,
        },
    )
    .expect("new mouse_velocity_profile_default key should parse");
    println!(
        "readback=profile_defaults edge=new_key after_mouse_velocity_profile_default={}",
        loaded.defaults.mouse_velocity_profile_default
    );
    assert_eq!(loaded.defaults.mouse_velocity_profile_default, "natural");

    let legacy_key = r#"
id = "legacy_curve_key"
label = "Legacy Curve Key"
schema_version = 2
use_scope = "productivity"
mouse_curve_default = "natural"
keyboard_dynamics_default = "natural"

[[matches]]
exe = "legacy_curve_key.exe"
"#;
    let loaded = parse_profile_bytes(
        "legacy_curve_key.toml",
        legacy_key.as_bytes(),
        UNIX_EPOCH,
        ScreenBounds {
            width: 1920,
            height: 1080,
        },
    )
    .expect("deprecated mouse_curve_default key should remain a hidden compatibility alias");
    println!(
        "readback=profile_defaults edge=legacy_key after_mouse_velocity_profile_default={}",
        loaded.defaults.mouse_velocity_profile_default
    );
    assert_eq!(loaded.defaults.mouse_velocity_profile_default, "natural");

    let both = r#"
id = "both_velocity_keys"
label = "Both Velocity Keys"
schema_version = 2
use_scope = "productivity"
mouse_velocity_profile_default = "natural"
mouse_curve_default = "natural"
keyboard_dynamics_default = "natural"

[[matches]]
exe = "both_velocity_keys.exe"
"#;
    let error = parse_profile_bytes(
        "both_velocity_keys.toml",
        both.as_bytes(),
        UNIX_EPOCH,
        ScreenBounds {
            width: 1920,
            height: 1080,
        },
    )
    .expect_err("profile must fail closed when both velocity default names are present");
    println!(
        "readback=profile_defaults edge=both_keys after=code:{} error:{}",
        error.code(),
        error
    );
    assert_eq!(error.code(), error_codes::PROFILE_PARSE_ERROR);
    assert!(error.to_string().contains("not both"));
}

#[test]
fn parser_rejects_legacy_schema_and_missing_use_scope() {
    let cases = [
        (
            "legacy_schema.toml",
            r#"
id = "legacy_schema"
label = "Legacy Schema"
schema_version = 1
use_scope = "productivity"
mouse_velocity_profile_default = "natural"
keyboard_dynamics_default = "natural"

[[matches]]
exe = "legacy.exe"
"#,
            error_codes::PROFILE_VERSION_INCOMPATIBLE,
        ),
        (
            "missing_use_scope.toml",
            r#"
id = "missing_use_scope"
label = "Missing Use Scope"
schema_version = 2
mouse_velocity_profile_default = "natural"
keyboard_dynamics_default = "natural"

[[matches]]
exe = "missing-use-scope.exe"
"#,
            error_codes::PROFILE_PARSE_ERROR,
        ),
    ];

    for (path, toml, code) in cases {
        println!("readback=profile_schema edge={path} before={toml}");
        let result = parse_profile_bytes(
            path,
            toml.as_bytes(),
            UNIX_EPOCH,
            ScreenBounds {
                width: 1920,
                height: 1080,
            },
        );
        let Err(error) = result else {
            panic!("{path} parsed successfully but should have failed");
        };
        println!(
            "readback=profile_schema edge={path} after=code:{} error:{}",
            error.code(),
            error
        );
        assert_eq!(error.code(), code, "{path}: {error}");
    }
}

#[test]
fn parser_accepts_default_backend_alias_for_action_backends() {
    let toml = r#"
id = "hardware_alias"
label = "Hardware Alias"
schema_version = 2
use_scope = "operator_owned_test"
mouse_velocity_profile_default = "natural"
keyboard_dynamics_default = "natural"

[[matches]]
exe = "hardware-alias.exe"

[backends]
default_backend = "hardware"
"#;

    let loaded = parse_profile_bytes(
        "hardware_alias.toml",
        toml.as_bytes(),
        UNIX_EPOCH,
        ScreenBounds {
            width: 1920,
            height: 1080,
        },
    )
    .unwrap_or_else(|error| panic!("default_backend alias should parse: {error}"));

    assert_eq!(loaded.profile.backends.default, Backend::Hardware);
    assert_eq!(loaded.profile.backends.keyboard_default, Backend::Auto);
    assert_eq!(loaded.profile.backends.mouse_default, Backend::Auto);
    assert_eq!(loaded.profile.backends.pad_default, Backend::Auto);
    println!(
        "readback=profile_backend_alias edge=default_backend before=default_backend after={:?} profile_id={}",
        loaded.profile.backends, loaded.profile.id
    );
}

#[test]
fn parser_accepts_mouse_button_keymap_aliases_and_metadata() {
    let toml = r#"
id = "mouse_aliases"
label = "Mouse Aliases"
schema_version = 2
use_scope = "operator_owned_test"
mouse_velocity_profile_default = "natural"
keyboard_dynamics_default = "natural"

[[matches]]
exe = "luanti.exe"

[keymap]
dig = "lmb"
place = "rmb"
pan = "mmb"

[metadata]
benchmark_id = "luanti.minetest"
"supported_use.local_world_only" = "true"
"#;

    let loaded = parse_profile_bytes(
        "mouse_aliases.toml",
        toml.as_bytes(),
        UNIX_EPOCH,
        ScreenBounds {
            width: 1920,
            height: 1080,
        },
    )
    .unwrap_or_else(|error| panic!("mouse button keymap aliases should parse: {error}"));

    assert_eq!(loaded.profile.keymap["dig"], "lmb");
    assert_eq!(loaded.profile.metadata["benchmark_id"], "luanti.minetest");
    assert_eq!(
        loaded.profile.metadata["supported_use.local_world_only"],
        "true"
    );
}

#[test]
fn parser_accepts_nested_hud_fields_and_event_extensions() {
    let toml = r#"
id = "luanti_nested"
label = "Luanti Nested"
schema_version = 2
use_scope = "operator_owned_test"
mode = "pixel_only"
mouse_velocity_profile_default = "natural"
keyboard_dynamics_default = "natural"

[[matches]]
exe = "luanti.exe"

[[hud]]
name = "luanti.crosshair_contrast"
extractor = { kind = "color_ratio", sample_points = [[0, 0], [24, 24]], mapping = "luma_stddev_0_1" }
parser = { kind = "number" }
region = { kind = "fraction_of_window", x = 0.48, y = 0.48, w = 0.04, h = 0.04 }

[[event_extensions]]
name = "luanti_hud_observed"
from_filter = { op = "and", args = [
    { op = "source", source = "perception_hud" },
    { op = "kind", kind = "perception.hud_observed" }
] }
emits_kind = "benchmark.luanti.hud_observed"

[[event_extensions]]
name = "luanti_entity_observed"
from_filter = { op = "and", args = [
    { op = "source", source = "perception_detection" },
    { op = "kind", kind = "entity_appeared" },
    { op = "data", path = "/profile_id", predicate = { op = "eq", value = "luanti.minetest" } }
] }
emits_kind = "benchmark.luanti.entity_observed"

[[event_extensions]]
name = "luanti_action_observed"
from_filter = { op = "and", args = [
    { op = "source", source = "action_emitter" },
    { op = "kind", kind = "action.dispatched" },
    { op = "data", path = "/tool", predicate = { op = "exists" } }
] }
emits_kind = "benchmark.luanti.action_observed"
"#;

    let loaded = parse_profile_bytes(
        "luanti_nested.toml",
        toml.as_bytes(),
        UNIX_EPOCH,
        ScreenBounds {
            width: 1920,
            height: 1080,
        },
    )
    .unwrap_or_else(|error| panic!("nested HUD/event profile should parse: {error}"));

    assert_eq!(loaded.profile.hud.len(), 1);
    assert_eq!(loaded.profile.hud[0].name, "luanti.crosshair_contrast");
    assert!(matches!(
        loaded.profile.hud[0].region,
        HudRegion::FractionOfWindow { .. }
    ));
    assert!(matches!(
        loaded.profile.hud[0].extractor,
        HudExtractor::ColorRatio { .. }
    ));
    assert_eq!(loaded.profile.event_extensions.len(), 3);
    assert_eq!(
        loaded.profile.event_extensions[0].emits_kind,
        "benchmark.luanti.hud_observed"
    );
    assert_eq!(
        loaded.profile.event_extensions[1].emits_kind,
        "benchmark.luanti.entity_observed"
    );
    assert_eq!(
        loaded.profile.event_extensions[2].emits_kind,
        "benchmark.luanti.action_observed"
    );
}

#[test]
fn parser_accepts_center_anchored_hud_region() {
    let toml = r#"
id = "center_hud"
label = "Center HUD"
schema_version = 2
use_scope = "operator_owned_test"
mouse_velocity_profile_default = "natural"
keyboard_dynamics_default = "natural"

[[matches]]
exe = "center-hud.exe"

[[hud]]
name = "center.prompt"
extractor = { kind = "winrt_ocr" }
parser = { kind = "number" }
region = { kind = "anchored_to_edge", edge = "center", x_offset = -40, y_offset = -12, w = 80, h = 24 }
"#;

    println!(
        "readback=profile_hud_anchor edge=center_parse before=toml_edge:center offsets=(-40,-12) size=(80,24)"
    );
    let loaded = parse_profile_bytes(
        "center_hud.toml",
        toml.as_bytes(),
        UNIX_EPOCH,
        ScreenBounds {
            width: 1920,
            height: 1080,
        },
    )
    .unwrap_or_else(|error| panic!("center anchored HUD region should parse: {error}"));
    println!(
        "readback=profile_hud_anchor edge=center_parse after=region:{:?}",
        loaded.profile.hud[0].region
    );

    assert!(matches!(
        loaded.profile.hud[0].region,
        HudRegion::AnchoredToEdge {
            edge: WindowEdge::Center,
            x_offset: -40,
            y_offset: -12,
            w: 80,
            h: 24,
        }
    ));
    assert_f32_close(
        loaded.profile.hud[0].confidence_threshold,
        default_hud_confidence_threshold(),
    );
}

#[test]
fn parser_accepts_hud_confidence_threshold_default_and_override() {
    let toml = r#"
id = "hud_threshold"
label = "HUD Threshold"
schema_version = 2
use_scope = "operator_owned_test"
mouse_velocity_profile_default = "natural"
keyboard_dynamics_default = "natural"

[[matches]]
exe = "hud-threshold.exe"

[[hud]]
name = "hp.default"
x = 10
y = 10
w = 100
h = 20

[[hud]]
name = "hp.custom"
confidence_threshold = 0.73
extractor = { kind = "winrt_ocr" }
parser = { kind = "regex", pattern = "([0-9]+)", group = 1 }
x = 10
y = 40
w = 100
h = 20
"#;

    println!("readback=profile_hud_threshold edge=parse before=default:absent custom:0.73");
    let loaded = parse_profile_bytes(
        "hud_threshold.toml",
        toml.as_bytes(),
        UNIX_EPOCH,
        ScreenBounds {
            width: 1920,
            height: 1080,
        },
    )
    .unwrap_or_else(|error| panic!("HUD confidence thresholds should parse: {error}"));
    println!(
        "readback=profile_hud_threshold edge=parse after=default:{:.2} custom:{:.2} parser:{:?}",
        loaded.profile.hud[0].confidence_threshold,
        loaded.profile.hud[1].confidence_threshold,
        loaded.profile.hud[1].parser
    );

    assert_f32_close(
        loaded.profile.hud[0].confidence_threshold,
        default_hud_confidence_threshold(),
    );
    assert_f32_close(loaded.profile.hud[1].confidence_threshold, 0.73);
    assert!(matches!(
        loaded.profile.hud[1].parser,
        HudParser::Regex { .. }
    ));
}

#[test]
fn parser_rejects_hud_confidence_threshold_outside_unit_interval() {
    let toml = r#"
id = "bad_hud_threshold"
label = "Bad HUD Threshold"
schema_version = 2
use_scope = "operator_owned_test"
mouse_velocity_profile_default = "natural"
keyboard_dynamics_default = "natural"

[[matches]]
exe = "bad-hud-threshold.exe"

[[hud]]
name = "hp.bad"
confidence_threshold = 1.01
x = 10
y = 10
w = 100
h = 20
"#;

    println!("readback=profile_hud_threshold edge=invalid before=confidence_threshold:1.01");
    let result = parse_profile_bytes(
        "bad_hud_threshold.toml",
        toml.as_bytes(),
        UNIX_EPOCH,
        ScreenBounds {
            width: 1920,
            height: 1080,
        },
    );
    let Err(error) = result else {
        panic!("out-of-range HUD confidence threshold parsed successfully");
    };
    println!(
        "readback=profile_hud_threshold edge=invalid after=code:{} error:{}",
        error.code(),
        error
    );
    assert_eq!(error.code(), error_codes::PROFILE_PARSE_ERROR);
    assert!(
        error
            .to_string()
            .contains("confidence_threshold must be finite and in 0..=1")
    );
}

#[test]
fn parser_rejects_trivially_true_event_extension_filter() {
    let toml = r#"
id = "always_true_event"
label = "Always True Event"
schema_version = 2
use_scope = "operator_owned_test"
mouse_velocity_profile_default = "natural"
keyboard_dynamics_default = "natural"

[[matches]]
exe = "always-true.exe"

[[event_extensions]]
name = "always_true"
from_filter = { op = "all" }
emits_kind = "always.true"
"#;

    let result = parse_profile_bytes(
        "always_true_event.toml",
        toml.as_bytes(),
        UNIX_EPOCH,
        ScreenBounds {
            width: 1920,
            height: 1080,
        },
    );
    let Err(error) = result else {
        panic!("always-true event extension parsed successfully but should fail");
    };
    println!(
        "readback=profile_event_extension edge=always_true after=code:{} error:{}",
        error.code(),
        error
    );
    assert_eq!(error.code(), error_codes::PROFILE_PARSE_ERROR);
    assert!(
        error
            .to_string()
            .contains("from_filter must not be trivially always true")
    );
}

#[test]
fn parser_rejects_hud_regions_outside_screen_bounds() {
    let toml = r#"
id = "bad_hud"
label = "Bad HUD"
schema_version = 2
use_scope = "productivity"
mouse_velocity_profile_default = "natural"
keyboard_dynamics_default = "natural"

[[matches]]
exe = "hud.exe"

[[hud]]
name = "hp"
x = 2000
y = 0
w = 100
h = 100
"#;

    let result = parse_profile_bytes(
        "bad_hud.toml",
        toml.as_bytes(),
        UNIX_EPOCH,
        ScreenBounds {
            width: 1920,
            height: 1080,
        },
    );
    let Err(error) = result else {
        panic!("HUD region parsed successfully but should be outside 1920x1080 bounds");
    };
    assert!(matches!(error, ProfileError::HudRegionInvalid { .. }));
    assert_eq!(error.code(), error_codes::PROFILE_HUD_REGION_INVALID);
}
