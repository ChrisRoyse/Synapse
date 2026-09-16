use proptest::{
    collection::vec,
    prelude::*,
    test_runner::{Config, TestCaseError, TestRng, TestRunner},
};
use synapse_action::{ActionBackend, EmitState, RecordedInput, RecordingBackend};
use synapse_core::{Action, Backend, Key, KeyCode, KeystrokeDynamics};

#[test]
fn known_mixed_case_records_shift_around_each_required_key()
-> Result<(), Box<dyn std::error::Error>> {
    let text = "aA!";
    let (before_events, events) = record_type_text_with_before(text)?;
    println!(
        "readback=dynamics_modifier_order edge=known_mixed input={text:?} before_events={before_events:?} after_events={events:?} after_trace={:?} result_value=events:{}",
        trace_window(&events, 0),
        events.len()
    );

    assert!(before_events.is_empty());
    assert_eq!(
        events,
        vec![
            key_down("a"),
            key_up("a"),
            key_down("shift"),
            key_down("a"),
            key_up("a"),
            key_up("shift"),
            key_down("shift"),
            key_down("1"),
            key_up("1"),
            key_up("shift"),
        ]
    );
    assert_modifier_order(&events).map_err(|error| format!("{error} events={events:?}"))?;
    Ok(())
}

#[test]
fn lowercase_edge_never_holds_shift() -> Result<(), Box<dyn std::error::Error>> {
    let text = "abc";
    let (before_events, events) = record_type_text_with_before(text)?;
    println!(
        "readback=dynamics_modifier_order edge=lowercase input={text:?} before_events={before_events:?} after_events={events:?} after_trace={:?} result_value=events:{}",
        trace_window(&events, 0),
        events.len()
    );

    assert!(before_events.is_empty());
    assert_eq!(
        events,
        vec![
            key_down("a"),
            key_up("a"),
            key_down("b"),
            key_up("b"),
            key_down("c"),
            key_up("c"),
        ]
    );
    assert_modifier_order(&events).map_err(|error| format!("{error} events={events:?}"))?;
    Ok(())
}

#[test]
fn consecutive_uppercase_releases_shift_between_characters()
-> Result<(), Box<dyn std::error::Error>> {
    let text = "AB";
    let (before_events, events) = record_type_text_with_before(text)?;
    println!(
        "readback=dynamics_modifier_order edge=consecutive_uppercase input={text:?} before_events={before_events:?} after_events={events:?} after_trace={:?} result_value=events:{}",
        trace_window(&events, 0),
        events.len()
    );

    assert!(before_events.is_empty());
    assert_eq!(
        events,
        vec![
            key_down("shift"),
            key_down("a"),
            key_up("a"),
            key_up("shift"),
            key_down("shift"),
            key_down("b"),
            key_up("b"),
            key_up("shift"),
        ]
    );
    assert_modifier_order(&events).map_err(|error| format!("{error} events={events:?}"))?;
    Ok(())
}

#[test]
fn random_mixed_case_strings_keep_shift_scoped_to_each_character_1000()
-> Result<(), Box<dyn std::error::Error>> {
    let config = Config {
        cases: 1_000,
        failure_persistence: None,
        ..Config::default()
    };
    let algorithm = config.rng_algorithm;
    let mut runner = TestRunner::new_with_rng(config, TestRng::deterministic_rng(algorithm));

    runner.run(&mixed_case_len_50_strategy(), |text| {
        let events = record_type_text(&text)
            .map_err(|error| TestCaseError::fail(format!("input={text:?} error={error}")))?;
        assert_modifier_order(&events).map_err(|error| {
            TestCaseError::fail(format!(
                "input={text:?} error={error} events={events:?} trace={:?}",
                trace_window(&events, 0)
            ))
        })?;
        Ok(())
    })?;

    println!(
        "readback=dynamics_modifier_order edge=proptest result_value=ok cases=1000 chars_per_case=50 alphabet=mixed_ascii_case"
    );
    Ok(())
}

fn mixed_case_len_50_strategy() -> impl Strategy<Value = String> {
    (
        prop::char::range('a', 'z'),
        prop::char::range('A', 'Z'),
        vec(
            prop_oneof![prop::char::range('a', 'z'), prop::char::range('A', 'Z'),],
            48,
        ),
    )
        .prop_map(|(lower, upper, rest)| {
            let mut chars = Vec::with_capacity(50);
            chars.push(lower);
            chars.push(upper);
            chars.extend(rest);
            chars.into_iter().collect()
        })
}

fn record_type_text(text: &str) -> Result<Vec<RecordedInput>, Box<dyn std::error::Error>> {
    let (_before_events, events) = record_type_text_with_before(text)?;
    Ok(events)
}

fn record_type_text_with_before(
    text: &str,
) -> Result<(Vec<RecordedInput>, Vec<RecordedInput>), Box<dyn std::error::Error>> {
    let backend = RecordingBackend::new();
    let mut state = EmitState::new();
    let before_events = backend.events();
    backend.execute(
        &Action::TypeText {
            text: text.to_owned(),
            dynamics: KeystrokeDynamics::Burst,
            backend: Backend::Software,
        },
        &mut state,
    )?;
    Ok((before_events, backend.events()))
}

fn assert_modifier_order(events: &[RecordedInput]) -> Result<(), String> {
    let mut index = 0;

    while index < events.len() {
        match events.get(index) {
            Some(RecordedInput::KeyDown { key }) if is_shift_key(key) => {
                let subsequence = &events[index..events.len().min(index + 4)];
                match subsequence {
                    [
                        RecordedInput::KeyDown { key: shift_down },
                        RecordedInput::KeyDown { key: char_down },
                        RecordedInput::KeyUp { key: char_up },
                        RecordedInput::KeyUp { key: shift_up },
                    ] if is_shift_key(shift_down)
                        && !is_shift_key(char_down)
                        && same_key(char_down, char_up)
                        && is_shift_key(shift_up) =>
                    {
                        index += 4;
                    }
                    _ => {
                        return Err(format!(
                            "bad_shift_subsequence_at={index} bad_subsequence={subsequence:?} modifier_trace={:?}",
                            trace_window(events, index)
                        ));
                    }
                }
            }
            Some(RecordedInput::KeyDown { key }) => match events.get(index + 1) {
                Some(RecordedInput::KeyUp { key: key_up }) if same_key(key, key_up) => {
                    index += 2;
                }
                _ => {
                    return Err(format!(
                        "bad_plain_key_subsequence_at={index} bad_subsequence={:?} modifier_trace={:?}",
                        &events[index..events.len().min(index + 2)],
                        trace_window(events, index)
                    ));
                }
            },
            Some(RecordedInput::UnicodeUnitDown { unit }) => match events.get(index + 1) {
                Some(RecordedInput::UnicodeUnitUp { unit: up_unit }) if unit == up_unit => {
                    index += 2;
                }
                _ => {
                    return Err(format!(
                        "bad_unicode_subsequence_at={index} bad_subsequence={:?} modifier_trace={:?}",
                        &events[index..events.len().min(index + 2)],
                        trace_window(events, index)
                    ));
                }
            },
            Some(event) => {
                return Err(format!(
                    "unexpected_event_at={index} event={event:?} modifier_trace={:?}",
                    trace_window(events, index)
                ));
            }
            None => break,
        }
    }

    Ok(())
}

fn trace_window(events: &[RecordedInput], center: usize) -> Vec<String> {
    let mut shift_held = false;
    let mut trace = Vec::new();
    let start = center.saturating_sub(4);
    let end = events.len().min(center.saturating_add(8));

    for (index, event) in events.iter().enumerate() {
        let shift_before = shift_held;
        match event {
            RecordedInput::KeyDown { key } if is_shift_key(key) => shift_held = true,
            RecordedInput::KeyUp { key } if is_shift_key(key) => shift_held = false,
            _ => {}
        }
        let shift_after = shift_held;
        if (start..end).contains(&index) {
            trace.push(format!(
                "index={index} shift_before={shift_before} shift_after={shift_after} event={event:?}"
            ));
        }
    }

    trace
}

fn key_down(value: &str) -> RecordedInput {
    RecordedInput::KeyDown { key: key(value) }
}

fn key_up(value: &str) -> RecordedInput {
    RecordedInput::KeyUp { key: key(value) }
}

fn key(value: &str) -> Key {
    Key {
        code: KeyCode::Named {
            value: value.to_owned(),
        },
        use_scancode: false,
    }
}

fn is_shift_key(key: &Key) -> bool {
    matches!(&key.code, KeyCode::Named { value } if value == "shift")
}

fn same_key(left: &Key, right: &Key) -> bool {
    left == right
}
