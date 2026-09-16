#![allow(clippy::missing_const_for_fn)]

#[path = "profile_types/fixtures.rs"]
mod fixtures;
#[path = "profile_types/helpers.rs"]
mod helpers;
#[path = "profile_types/strategies.rs"]
mod strategies;

use fixtures::*;
use helpers::{assert_strategy_round_trips, round_trip};
use strategies::{
    event_extension_strategy, hud_extractor_strategy, hud_field_spec_strategy, hud_parser_strategy,
    hud_region_strategy, profile_backends_strategy, profile_capture_strategy,
    profile_capture_target_strategy, profile_detection_strategy, profile_match_strategy,
    profile_ocr_strategy, profile_strategy, profile_use_scope_strategy, window_edge_strategy,
};
use synapse_core::{ProfileCaptureTarget, ProfileUseScope, WindowEdge};

#[test]
fn profile_type_edge_round_trips_with_readback() -> Result<(), Box<dyn std::error::Error>> {
    round_trip("Profile", "empty", empty_profile("empty"))?;
    round_trip("Profile", "required_only", required_profile("required"))?;
    round_trip("Profile", "fully_populated", full_profile())?;

    round_trip("ProfileMatch", "empty", empty_profile_match())?;
    round_trip(
        "ProfileMatch",
        "required_only",
        exe_profile_match("notepad.exe"),
    )?;
    round_trip("ProfileMatch", "fully_populated", full_profile_match())?;

    round_trip("ProfileCapture", "empty", foreground_capture())?;
    round_trip("ProfileCapture", "required_only", primary_monitor_capture())?;
    round_trip(
        "ProfileCapture",
        "fully_populated",
        monitor_index_capture(2),
    )?;

    round_trip("ProfileDetection", "empty", disabled_detection())?;
    round_trip("ProfileDetection", "required_only", minimal_detection())?;
    round_trip("ProfileDetection", "fully_populated", full_detection())?;

    round_trip("ProfileOcr", "empty", empty_ocr())?;
    round_trip("ProfileOcr", "required_only", winrt_ocr())?;
    round_trip("ProfileOcr", "fully_populated", full_ocr())?;

    round_trip("HudFieldSpec", "empty", minimal_hud_field("empty"))?;
    round_trip(
        "HudFieldSpec",
        "required_only",
        minimal_hud_field("required"),
    )?;
    round_trip("HudFieldSpec", "fully_populated", full_hud_field())?;

    round_trip("ProfileBackends", "empty", software_backends())?;
    round_trip("ProfileBackends", "required_only", software_backends())?;
    round_trip("ProfileBackends", "fully_populated", mixed_backends())?;

    round_trip("EventExtension", "empty", minimal_event_extension("empty"))?;
    round_trip(
        "EventExtension",
        "required_only",
        minimal_event_extension("required"),
    )?;
    round_trip("EventExtension", "fully_populated", full_event_extension())?;

    Ok(())
}

#[test]
fn profile_type_json_snapshots() -> Result<(), Box<dyn std::error::Error>> {
    insta::assert_json_snapshot!(
        "profile_round_trip",
        round_trip("Profile", "snapshot", full_profile())?
    );
    insta::assert_json_snapshot!(
        "profile_match_round_trip",
        round_trip("ProfileMatch", "snapshot", full_profile_match())?
    );
    insta::assert_json_snapshot!(
        "profile_capture_round_trip",
        round_trip("ProfileCapture", "snapshot", monitor_index_capture(2))?
    );
    insta::assert_json_snapshot!(
        "profile_capture_target_round_trip",
        round_trip(
            "ProfileCaptureTarget",
            "snapshot",
            ProfileCaptureTarget::MonitorIndex { index: 2 },
        )?
    );
    insta::assert_json_snapshot!(
        "profile_detection_round_trip",
        round_trip("ProfileDetection", "snapshot", full_detection())?
    );
    insta::assert_json_snapshot!(
        "profile_ocr_round_trip",
        round_trip("ProfileOcr", "snapshot", full_ocr())?
    );
    insta::assert_json_snapshot!(
        "hud_field_spec_round_trip",
        round_trip("HudFieldSpec", "snapshot", full_hud_field())?
    );
    insta::assert_json_snapshot!(
        "hud_region_round_trip",
        round_trip("HudRegion", "snapshot", anchored_region())?
    );
    insta::assert_json_snapshot!(
        "window_edge_round_trip",
        round_trip("WindowEdge", "snapshot", WindowEdge::BottomLeft)?
    );
    insta::assert_json_snapshot!(
        "hud_extractor_round_trip",
        round_trip("HudExtractor", "snapshot", full_extractor())?
    );
    insta::assert_json_snapshot!(
        "hud_parser_round_trip",
        round_trip("HudParser", "snapshot", full_parser())?
    );
    insta::assert_json_snapshot!(
        "profile_backends_round_trip",
        round_trip("ProfileBackends", "snapshot", mixed_backends())?
    );
    insta::assert_json_snapshot!(
        "event_extension_round_trip",
        round_trip("EventExtension", "snapshot", full_event_extension())?
    );
    insta::assert_json_snapshot!(
        "profile_use_scope_round_trip",
        round_trip(
            "ProfileUseScope",
            "snapshot",
            ProfileUseScope::OperatorOwnedTest,
        )?
    );

    Ok(())
}

#[test]
fn profile_types_proptest_json_round_trip_is_deterministic()
-> Result<(), Box<dyn std::error::Error>> {
    assert_strategy_round_trips("Profile", profile_strategy())?;
    assert_strategy_round_trips("ProfileMatch", profile_match_strategy())?;
    assert_strategy_round_trips("ProfileCapture", profile_capture_strategy())?;
    assert_strategy_round_trips("ProfileCaptureTarget", profile_capture_target_strategy())?;
    assert_strategy_round_trips("ProfileDetection", profile_detection_strategy())?;
    assert_strategy_round_trips("ProfileOcr", profile_ocr_strategy())?;
    assert_strategy_round_trips("HudFieldSpec", hud_field_spec_strategy())?;
    assert_strategy_round_trips("HudRegion", hud_region_strategy())?;
    assert_strategy_round_trips("WindowEdge", window_edge_strategy())?;
    assert_strategy_round_trips("HudExtractor", hud_extractor_strategy())?;
    assert_strategy_round_trips("HudParser", hud_parser_strategy())?;
    assert_strategy_round_trips("ProfileBackends", profile_backends_strategy())?;
    assert_strategy_round_trips("EventExtension", event_extension_strategy())?;
    assert_strategy_round_trips("ProfileUseScope", profile_use_scope_strategy())?;
    Ok(())
}
