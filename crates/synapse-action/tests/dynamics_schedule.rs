use synapse_action::{BIGRAMS, ModifierMask, sample_typing_schedule};
use synapse_core::{KeyCode, KeystrokeDynamics, KeystrokeNaturalParams};

#[test]
fn burst_schedule_has_zero_iki_and_empty_input_stays_empty() {
    let empty_before = "";
    let empty = sample_typing_schedule(empty_before, &KeystrokeDynamics::Burst, Some(7));
    println!(
        "readback=dynamics_schedule edge=empty before={empty_before:?} after={empty:?} result_value={}",
        empty.len()
    );
    assert!(empty.is_empty());

    let text = "abc";
    let schedule = sample_typing_schedule(text, &KeystrokeDynamics::Burst, Some(7));
    let ikis: Vec<_> = schedule.iter().map(|event| event.iki_ms_before).collect();
    println!(
        "readback=dynamics_schedule edge=burst before={text:?} after={schedule:?} result_value={ikis:?}"
    );

    assert_eq!(schedule.len(), 3);
    assert_eq!(ikis, [0, 0, 0]);
    assert!(
        schedule
            .iter()
            .all(|event| event.modifier_state == ModifierMask::NONE)
    );
}

#[test]
fn linear_schedule_spaces_after_first_character_and_records_shift() {
    let text = "Aa!";
    let dynamics = KeystrokeDynamics::Linear { ms_per_char: 17 };
    let schedule = sample_typing_schedule(text, &dynamics, Some(99));
    let ikis: Vec<_> = schedule.iter().map(|event| event.iki_ms_before).collect();
    let modifiers: Vec<_> = schedule
        .iter()
        .map(|event| event.modifier_state.bits())
        .collect();
    println!(
        "readback=dynamics_schedule edge=linear_shift before={text:?} after={schedule:?} result_value=ikis:{ikis:?},modifiers:{modifiers:?}"
    );

    assert_eq!(ikis, [0, 17, 17]);
    assert_eq!(schedule[0].r#char, 'A');
    assert_eq!(named_key_value(&schedule[0]), Some("a"));
    assert!(schedule[0].modifier_state.contains(ModifierMask::SHIFT));
    assert_eq!(schedule[1].r#char, 'a');
    assert_eq!(named_key_value(&schedule[1]), Some("a"));
    assert!(schedule[1].modifier_state.is_empty());
    assert_eq!(schedule[2].r#char, '!');
    assert_eq!(named_key_value(&schedule[2]), Some("1"));
    assert!(schedule[2].modifier_state.contains(ModifierMask::SHIFT));
}

#[test]
fn natural_schedule_applies_compile_time_bigram_bias_and_seed_determinism() {
    assert!(BIGRAMS.contains(&"th"));
    assert!(BIGRAMS.contains(&"he"));
    assert!(BIGRAMS.contains(&"in"));

    let text = "thez";
    let dynamics = KeystrokeDynamics::Natural {
        params: KeystrokeNaturalParams::FAST,
    };
    let first = sample_typing_schedule(text, &dynamics, Some(42));
    let second = sample_typing_schedule(text, &dynamics, Some(42));
    let different_seed = sample_typing_schedule(text, &dynamics, Some(43));
    let ikis: Vec<_> = first.iter().map(|event| event.iki_ms_before).collect();
    let different_seed_ikis: Vec<_> = different_seed
        .iter()
        .map(|event| event.iki_ms_before)
        .collect();
    println!(
        "readback=dynamics_schedule edge=natural_seeded before={text:?} after={first:?} result_value=ikis:{ikis:?},different_seed:{different_seed_ikis:?}"
    );

    assert_eq!(first, second);
    assert_eq!(ikis, [0, 24, 24, 36]);
    assert_eq!(different_seed_ikis, [0, 24, 24, 26]);
}

#[test]
fn natural_schedule_sanitizes_non_finite_params_without_panicking() {
    let text = "xy";
    let dynamics = KeystrokeDynamics::Natural {
        params: KeystrokeNaturalParams {
            mean_iki_ms: f32::NAN,
            stddev_ms: f32::INFINITY,
            bigram_bias: false,
        },
    };
    let schedule = sample_typing_schedule(text, &dynamics, None);
    let ikis: Vec<_> = schedule.iter().map(|event| event.iki_ms_before).collect();
    println!(
        "readback=dynamics_schedule edge=non_finite before={text:?} after={schedule:?} result_value={ikis:?}"
    );

    assert_eq!(ikis, [0, 0]);
}

const fn named_key_value(event: &synapse_action::KeystrokeEvent) -> Option<&str> {
    match &event.key.code {
        KeyCode::Named { value } => Some(value.as_str()),
        KeyCode::Symbol { .. } | KeyCode::HidCode { .. } => None,
    }
}
