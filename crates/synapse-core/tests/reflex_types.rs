#![allow(clippy::missing_const_for_fn)]

#[path = "reflex_types/fixtures.rs"]
mod fixtures;
#[path = "reflex_types/helpers.rs"]
mod helpers;
#[path = "reflex_types/strategies.rs"]
mod strategies;

use fixtures::*;
use helpers::{assert_strategy_round_trips, round_trip};
use strategies::{
    reflex_aim_axis_strategy, reflex_button_target_strategy, reflex_kind_strategy,
    reflex_lifetime_strategy, reflex_registration_strategy, reflex_state_strategy,
    reflex_status_strategy, reflex_then_strategy,
};
use synapse_core::{
    MouseButton, PadButton, ReflexAimAxis, ReflexButtonTarget, ReflexLifetime, ReflexState,
};

#[test]
fn reflex_type_edge_round_trips_with_readback() -> Result<(), Box<dyn std::error::Error>> {
    round_trip("ReflexRegistration", "empty", empty_registration())?;
    round_trip(
        "ReflexRegistration",
        "required_only",
        required_registration(),
    )?;
    round_trip("ReflexRegistration", "fully_populated", full_registration())?;

    round_trip("ReflexKind", "empty", empty_kind())?;
    round_trip("ReflexKind", "required_only", hold_move_kind())?;
    round_trip("ReflexKind", "fully_populated", full_on_event_kind())?;

    round_trip("ReflexLifetime", "empty", ReflexLifetime::UntilCancelled)?;
    round_trip("ReflexLifetime", "required_only", ReflexLifetime::OneShot)?;
    round_trip("ReflexLifetime", "fully_populated", full_lifetime())?;

    round_trip("ReflexState", "empty", ReflexState::Active)?;
    round_trip("ReflexState", "required_only", ReflexState::Paused)?;
    round_trip("ReflexState", "fully_populated", ReflexState::Starved)?;

    round_trip("ReflexStatus", "empty", empty_status())?;
    round_trip("ReflexStatus", "required_only", required_status())?;
    round_trip("ReflexStatus", "fully_populated", full_status())?;

    round_trip("ReflexThen", "empty", empty_then())?;
    round_trip("ReflexThen", "required_only", action_then("space"))?;
    round_trip("ReflexThen", "fully_populated", full_then())?;

    round_trip(
        "ReflexButtonTarget",
        "empty",
        ReflexButtonTarget::Mouse {
            button: MouseButton::Left,
        },
    )?;
    round_trip(
        "ReflexButtonTarget",
        "required_only",
        ReflexButtonTarget::Mouse {
            button: MouseButton::Right,
        },
    )?;
    round_trip(
        "ReflexButtonTarget",
        "fully_populated",
        ReflexButtonTarget::Pad {
            pad: 1,
            button: PadButton::Rb,
        },
    )?;

    round_trip("ReflexAimAxis", "empty", ReflexAimAxis::Xy)?;
    round_trip("ReflexAimAxis", "required_only", ReflexAimAxis::XOnly)?;
    round_trip("ReflexAimAxis", "fully_populated", ReflexAimAxis::YOnly)?;

    Ok(())
}

#[test]
fn reflex_type_json_snapshots() -> Result<(), Box<dyn std::error::Error>> {
    insta::assert_json_snapshot!(
        "reflex_registration_round_trip",
        round_trip("ReflexRegistration", "snapshot", full_registration())?
    );
    insta::assert_json_snapshot!(
        "reflex_kind_round_trip",
        round_trip("ReflexKind", "snapshot", full_on_event_kind())?
    );
    insta::assert_json_snapshot!(
        "reflex_lifetime_round_trip",
        round_trip("ReflexLifetime", "snapshot", full_lifetime())?
    );
    insta::assert_json_snapshot!(
        "reflex_state_round_trip",
        round_trip("ReflexState", "snapshot", ReflexState::Starved)?
    );
    insta::assert_json_snapshot!(
        "reflex_status_round_trip",
        round_trip("ReflexStatus", "snapshot", full_status())?
    );
    insta::assert_json_snapshot!(
        "reflex_then_round_trip",
        round_trip("ReflexThen", "snapshot", full_then())?
    );
    insta::assert_json_snapshot!(
        "reflex_button_target_round_trip",
        round_trip(
            "ReflexButtonTarget",
            "snapshot",
            ReflexButtonTarget::Pad {
                pad: 1,
                button: PadButton::Rb,
            },
        )?
    );
    insta::assert_json_snapshot!(
        "reflex_aim_axis_round_trip",
        round_trip("ReflexAimAxis", "snapshot", ReflexAimAxis::YOnly)?
    );

    Ok(())
}

#[test]
fn reflex_types_proptest_json_round_trip_is_deterministic() -> Result<(), Box<dyn std::error::Error>>
{
    assert_strategy_round_trips("ReflexRegistration", reflex_registration_strategy())?;
    assert_strategy_round_trips("ReflexKind", reflex_kind_strategy())?;
    assert_strategy_round_trips("ReflexLifetime", reflex_lifetime_strategy())?;
    assert_strategy_round_trips("ReflexState", reflex_state_strategy())?;
    assert_strategy_round_trips("ReflexStatus", reflex_status_strategy())?;
    assert_strategy_round_trips("ReflexThen", reflex_then_strategy())?;
    assert_strategy_round_trips("ReflexButtonTarget", reflex_button_target_strategy())?;
    assert_strategy_round_trips("ReflexAimAxis", reflex_aim_axis_strategy())?;
    Ok(())
}
