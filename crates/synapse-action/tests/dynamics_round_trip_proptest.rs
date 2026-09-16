use proptest::{
    collection::vec,
    prelude::*,
    test_runner::{Config, TestCaseError, TestRng, TestRunner},
};
use synapse_action::{
    ActionBackend, EmitState, RecordedInput, RecordingBackend, sample_typing_schedule,
};
use synapse_core::{Action, Backend, KeyCode, KeystrokeDynamics, KeystrokeNaturalParams};

#[test]
fn empty_string_records_zero_events() -> Result<(), Box<dyn std::error::Error>> {
    let text = "";
    let truth = record_and_reconstruct(text)?;
    println!(
        "readback=dynamics_round_trip edge=empty before={text:?} after=events:{:?},reconstructed:{:?} result_value=events:{}",
        truth.events,
        truth.reconstructed,
        truth.events.len()
    );

    assert!(truth.schedule_chars.is_empty());
    assert!(truth.schedule_ikis.is_empty());
    assert!(truth.events.is_empty());
    assert_eq!(truth.reconstructed, text);
    Ok(())
}

#[test]
fn extended_latin_string_round_trips() -> Result<(), Box<dyn std::error::Error>> {
    let text = "Az ÀĿſƀ";
    let truth = record_and_reconstruct(text)?;
    println!(
        "readback=dynamics_round_trip edge=extended_latin before={text:?} after=schedule:{:?},events:{:?},reconstructed:{:?} result_value={:?}",
        truth.schedule_chars, truth.events, truth.reconstructed, truth.reconstructed
    );

    assert_eq!(truth.schedule_chars, text);
    assert_eq!(truth.reconstructed, text);
    assert_eq!(
        unicode_down_count(&truth.events),
        expected_unicode_units(text)
    );
    assert_eq!(
        delay_ikis(&truth.events),
        non_zero_ikis(&truth.schedule_ikis)
    );
    assert_eq!(
        truth.events.len(),
        expected_recorded_event_count(text, &truth.schedule_ikis)
    );
    Ok(())
}

#[test]
fn random_strings_round_trip_through_recording_backend_10k()
-> Result<(), Box<dyn std::error::Error>> {
    let config = Config {
        cases: 10_000,
        failure_persistence: None,
        ..Config::default()
    };
    let algorithm = config.rng_algorithm;
    let mut runner = TestRunner::new_with_rng(config, TestRng::deterministic_rng(algorithm));

    runner.run(&text_strategy(), |text| {
        let truth = record_and_reconstruct(&text)
            .map_err(|error| TestCaseError::fail(format!("input={text:?} error={error}")))?;

        prop_assert_eq!(
            &truth.schedule_chars,
            &text,
            "input={:?} schedule={:?} events={:?}",
            text,
            truth.schedule_chars,
            truth.events
        );
        prop_assert_eq!(
            unicode_down_count(&truth.events),
            expected_unicode_units(&text),
            "input={:?} events={:?}",
            text,
            truth.events
        );
        prop_assert_eq!(
            delay_ikis(&truth.events),
            non_zero_ikis(&truth.schedule_ikis),
            "input={:?} ikis={:?} events={:?}",
            text,
            truth.schedule_ikis,
            truth.events
        );
        prop_assert_eq!(
            truth.events.len(),
            expected_recorded_event_count(&text, &truth.schedule_ikis),
            "input={:?} events={:?}",
            text,
            truth.events
        );
        prop_assert_eq!(
            &truth.reconstructed,
            &text,
            "input={:?} events={:?}",
            text,
            truth.events
        );
        Ok(())
    })?;

    println!(
        "readback=dynamics_round_trip edge=proptest result_value=ok cases=10000 max_chars=200 unicode_range=U+00C0..U+017F"
    );
    Ok(())
}

fn text_strategy() -> impl Strategy<Value = String> {
    vec(
        prop_oneof![
            prop::char::range('\u{0000}', '\u{007f}'),
            prop::char::range('\u{00c0}', '\u{017f}'),
        ],
        0..=200,
    )
    .prop_map(|chars| chars.into_iter().collect())
}

struct RoundTripTruth {
    schedule_chars: String,
    schedule_ikis: Vec<u32>,
    events: Vec<RecordedInput>,
    reconstructed: String,
}

fn record_and_reconstruct(text: &str) -> Result<RoundTripTruth, Box<dyn std::error::Error>> {
    let dynamics = KeystrokeDynamics::Natural {
        params: KeystrokeNaturalParams::FAST,
    };
    let schedule = sample_typing_schedule(text, &dynamics, None);
    let schedule_chars: String = schedule.iter().map(|event| event.r#char).collect();
    let schedule_ikis = schedule.iter().map(|event| event.iki_ms_before).collect();
    let backend = RecordingBackend::new();
    let mut state = EmitState::new();
    backend.execute(
        &Action::TypeText {
            text: text.to_owned(),
            dynamics,
            backend: Backend::Software,
        },
        &mut state,
    )?;
    let events = backend.events();
    let reconstructed = reconstruct_typed_text(&events)?;
    Ok(RoundTripTruth {
        schedule_chars,
        schedule_ikis,
        events,
        reconstructed,
    })
}

fn reconstruct_typed_text(events: &[RecordedInput]) -> Result<String, Box<dyn std::error::Error>> {
    let mut output = String::new();
    let mut unicode_units = Vec::new();
    let mut shift_held = false;

    for event in events {
        match event {
            RecordedInput::UnicodeUnitDown { unit } => unicode_units.push(*unit),
            RecordedInput::KeyDown { key }
                if key_name(key.code.clone()).as_deref() == Some("shift") =>
            {
                flush_unicode_units(&mut unicode_units, &mut output)?;
                shift_held = true;
            }
            RecordedInput::KeyUp { key }
                if key_name(key.code.clone()).as_deref() == Some("shift") =>
            {
                flush_unicode_units(&mut unicode_units, &mut output)?;
                shift_held = false;
            }
            RecordedInput::KeyDown { key } => {
                flush_unicode_units(&mut unicode_units, &mut output)?;
                if let Some(ch) = typed_char_for_key(&key.code, shift_held) {
                    output.push(ch);
                }
            }
            RecordedInput::KeyUp { .. }
            | RecordedInput::DelayMs { .. }
            | RecordedInput::UnicodeUnitUp { .. }
            | RecordedInput::MouseMove { .. }
            | RecordedInput::MouseMoveAbsolute { .. }
            | RecordedInput::MouseMoveRelative { .. }
            | RecordedInput::MouseButtonDown { .. }
            | RecordedInput::MouseButtonUp { .. }
            | RecordedInput::MouseStrokePoint { .. }
            | RecordedInput::MouseScroll { .. }
            | RecordedInput::AimAt { .. }
            | RecordedInput::ComboAt { .. }
            | RecordedInput::PadButtonDown { .. }
            | RecordedInput::PadButtonUp { .. }
            | RecordedInput::PadStick { .. }
            | RecordedInput::PadTrigger { .. }
            | RecordedInput::PadReport { .. }
            | RecordedInput::ReleaseAll { .. } => {}
        }
    }

    flush_unicode_units(&mut unicode_units, &mut output)?;
    Ok(output)
}

fn flush_unicode_units(
    units: &mut Vec<u16>,
    output: &mut String,
) -> Result<(), Box<dyn std::error::Error>> {
    if !units.is_empty() {
        output.push_str(&String::from_utf16(units)?);
        units.clear();
    }
    Ok(())
}

fn typed_char_for_key(code: &KeyCode, shift_held: bool) -> Option<char> {
    let name = key_name(code.clone())?;
    match name.as_str() {
        "space" => Some(' '),
        "tab" => Some('\t'),
        "enter" => Some('\n'),
        value if value.len() == 1 => {
            let ch = value.chars().next()?;
            Some(if shift_held {
                shifted_char(ch).unwrap_or_else(|| ch.to_ascii_uppercase())
            } else {
                ch
            })
        }
        _ => None,
    }
}

fn key_name(code: KeyCode) -> Option<String> {
    match code {
        KeyCode::Named { value } => Some(value),
        KeyCode::Symbol { value } => Some(value.to_string()),
        KeyCode::HidCode { .. } => None,
    }
}

const fn shifted_char(ch: char) -> Option<char> {
    match ch {
        '1' => Some('!'),
        '2' => Some('@'),
        '3' => Some('#'),
        '4' => Some('$'),
        '5' => Some('%'),
        '6' => Some('^'),
        '7' => Some('&'),
        '8' => Some('*'),
        '9' => Some('('),
        '0' => Some(')'),
        '-' => Some('_'),
        '=' => Some('+'),
        '[' => Some('{'),
        ']' => Some('}'),
        '\\' => Some('|'),
        ';' => Some(':'),
        '\'' => Some('"'),
        ',' => Some('<'),
        '.' => Some('>'),
        '/' => Some('?'),
        '`' => Some('~'),
        _ => None,
    }
}

fn unicode_down_count(events: &[RecordedInput]) -> usize {
    events
        .iter()
        .filter(|event| matches!(event, RecordedInput::UnicodeUnitDown { .. }))
        .count()
}

fn expected_unicode_units(text: &str) -> usize {
    text.chars()
        .filter(|ch| !is_reversible_key_character(*ch))
        .map(char::len_utf16)
        .sum()
}

fn expected_recorded_event_count(text: &str, schedule_ikis: &[u32]) -> usize {
    text.chars()
        .map(|ch| {
            if is_reversible_key_character(ch) {
                2 + usize::from(requires_shift(ch)) * 2
            } else {
                ch.len_utf16() * 2
            }
        })
        .sum::<usize>()
        + schedule_ikis.iter().filter(|iki| **iki > 0).count()
}

fn delay_ikis(events: &[RecordedInput]) -> Vec<u32> {
    events
        .iter()
        .filter_map(|event| match event {
            RecordedInput::DelayMs { ms } => Some(*ms),
            _ => None,
        })
        .collect()
}

fn non_zero_ikis(ikis: &[u32]) -> Vec<u32> {
    ikis.iter().copied().filter(|iki| *iki > 0).collect()
}

const fn requires_shift(ch: char) -> bool {
    matches!(
        ch,
        'A'..='Z'
            | '!'
            | '@'
            | '#'
            | '$'
            | '%'
            | '^'
            | '&'
            | '*'
            | '('
            | ')'
            | '_'
            | '+'
            | '{'
            | '}'
            | '|'
            | ':'
            | '"'
            | '<'
            | '>'
            | '?'
            | '~'
    )
}

const fn is_reversible_key_character(ch: char) -> bool {
    matches!(
        ch,
        'A'..='Z'
            | 'a'..='z'
            | '0'..='9'
            | '\n'
            | '\t'
            | ' '
            | '!'
            | '@'
            | '#'
            | '$'
            | '%'
            | '^'
            | '&'
            | '*'
            | '('
            | ')'
            | '_'
            | '+'
            | '{'
            | '}'
            | '|'
            | ':'
            | '"'
            | '<'
            | '>'
            | '?'
            | '~'
            | '-'
            | '='
            | '['
            | ']'
            | '\\'
            | ';'
            | '\''
            | ','
            | '.'
            | '/'
            | '`'
    )
}
