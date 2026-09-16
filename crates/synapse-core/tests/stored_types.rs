#![allow(clippy::missing_const_for_fn)]

#[path = "stored_types/fixtures.rs"]
mod fixtures;
#[path = "stored_types/helpers.rs"]
mod helpers;
#[path = "stored_types/strategies.rs"]
mod strategies;

use fixtures::*;
use helpers::{assert_strategy_round_trips, reject_unknown_field, round_trip};
use strategies::{
    stored_event_strategy, stored_observation_strategy, stored_profile_history_entry_strategy,
    stored_redaction_strategy, stored_reflex_audit_strategy, stored_reflex_step_strategy,
    stored_session_strategy,
};

#[test]
fn stored_type_edge_round_trips_with_readback() -> Result<(), Box<dyn std::error::Error>> {
    round_trip("StoredEvent", "empty", empty_event())?;
    round_trip("StoredEvent", "required_only", required_event())?;
    round_trip("StoredEvent", "fully_populated", full_event())?;

    round_trip("StoredObservation", "empty", empty_observation())?;
    round_trip("StoredObservation", "required_only", required_observation())?;
    round_trip("StoredObservation", "fully_populated", full_observation())?;

    round_trip("StoredReflexAudit", "empty", empty_reflex_audit())?;
    round_trip(
        "StoredReflexAudit",
        "required_only",
        required_reflex_audit(),
    )?;
    round_trip("StoredReflexAudit", "fully_populated", full_reflex_audit())?;

    round_trip("StoredSession", "empty", empty_session())?;
    round_trip("StoredSession", "required_only", required_session())?;
    round_trip("StoredSession", "fully_populated", full_session())?;

    round_trip("StoredRedaction", "empty", empty_redaction())?;
    round_trip("StoredRedaction", "required_only", required_redaction())?;
    round_trip("StoredRedaction", "fully_populated", full_redaction())?;

    round_trip("StoredReflexStep", "empty", empty_reflex_step())?;
    round_trip("StoredReflexStep", "required_only", required_reflex_step())?;
    round_trip("StoredReflexStep", "fully_populated", full_reflex_step())?;

    round_trip(
        "StoredProfileHistoryEntry",
        "empty",
        empty_profile_history_entry(),
    )?;
    round_trip(
        "StoredProfileHistoryEntry",
        "required_only",
        required_profile_history_entry(),
    )?;
    round_trip(
        "StoredProfileHistoryEntry",
        "fully_populated",
        full_profile_history_entry(),
    )?;

    Ok(())
}

#[test]
fn stored_type_json_snapshots() -> Result<(), Box<dyn std::error::Error>> {
    insta::assert_json_snapshot!(
        "stored_event_round_trip",
        round_trip("StoredEvent", "snapshot", full_event())?
    );
    insta::assert_json_snapshot!(
        "stored_observation_round_trip",
        round_trip("StoredObservation", "snapshot", full_observation())?
    );
    insta::assert_json_snapshot!(
        "stored_reflex_audit_round_trip",
        round_trip("StoredReflexAudit", "snapshot", full_reflex_audit())?
    );
    insta::assert_json_snapshot!(
        "stored_session_round_trip",
        round_trip("StoredSession", "snapshot", full_session())?
    );
    insta::assert_json_snapshot!(
        "stored_redaction_round_trip",
        round_trip("StoredRedaction", "snapshot", full_redaction())?
    );
    insta::assert_json_snapshot!(
        "stored_reflex_step_round_trip",
        round_trip("StoredReflexStep", "snapshot", full_reflex_step())?
    );
    insta::assert_json_snapshot!(
        "stored_profile_history_entry_round_trip",
        round_trip(
            "StoredProfileHistoryEntry",
            "snapshot",
            full_profile_history_entry(),
        )?
    );

    Ok(())
}

#[test]
fn stored_types_reject_unknown_fields() -> Result<(), Box<dyn std::error::Error>> {
    reject_unknown_field("StoredEvent", full_event())?;
    reject_unknown_field("StoredObservation", full_observation())?;
    reject_unknown_field("StoredReflexAudit", full_reflex_audit())?;
    reject_unknown_field("StoredSession", full_session())?;
    Ok(())
}

#[test]
fn stored_types_proptest_json_round_trip_is_deterministic() -> Result<(), Box<dyn std::error::Error>>
{
    assert_strategy_round_trips("StoredEvent", stored_event_strategy())?;
    assert_strategy_round_trips("StoredObservation", stored_observation_strategy())?;
    assert_strategy_round_trips("StoredReflexAudit", stored_reflex_audit_strategy())?;
    assert_strategy_round_trips("StoredSession", stored_session_strategy())?;
    assert_strategy_round_trips("StoredRedaction", stored_redaction_strategy())?;
    assert_strategy_round_trips("StoredReflexStep", stored_reflex_step_strategy())?;
    assert_strategy_round_trips(
        "StoredProfileHistoryEntry",
        stored_profile_history_entry_strategy(),
    )?;
    Ok(())
}
