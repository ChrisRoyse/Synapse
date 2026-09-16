use synapse_action::{KeystrokeEvent, sample_typing_schedule};
use synapse_core::{KeystrokeDynamics, KeystrokeNaturalParams};

const TEXT: &str = "Hello world.";
const SEED: u64 = 42;
const MAX_TOTAL_WALL_MS: u32 = 400;

#[test]
fn natural_fast_seed_42_hello_world_iki_snapshot() {
    let dynamics = natural_fast();
    let before = TEXT;
    let schedule = sample_typing_schedule(before, &dynamics, Some(SEED));
    let ikis = ikis_ms(&schedule);
    let total_wall_ms = total_wall_ms(&ikis);

    println!(
        "readback=dynamics_natural_hello_world edge=happy before=text:{before:?},seed:{SEED},dynamics:Natural::FAST after_schedule={schedule:?} result_value=ikis:{ikis:?},total_wall_ms:{total_wall_ms}"
    );

    assert_eq!(schedule.len(), TEXT.chars().count());
    assert_eq!(schedule.first().map(|event| event.iki_ms_before), Some(0));
    assert!(
        total_wall_ms <= MAX_TOTAL_WALL_MS,
        "total_wall_ms={total_wall_ms} exceeded {MAX_TOTAL_WALL_MS}; ikis={ikis:?}"
    );

    insta::with_settings!({ prepend_module_to_snapshot => false }, {
        insta::assert_snapshot!("dynamics_natural_hello_world", snapshot_text(&ikis, total_wall_ms));
    });
}

#[test]
fn empty_input_has_empty_iki_vector() {
    let dynamics = natural_fast();
    let before = "";
    let schedule = sample_typing_schedule(before, &dynamics, Some(SEED));
    let ikis = ikis_ms(&schedule);
    println!(
        "readback=dynamics_natural_hello_world edge=empty before=text:{before:?},seed:{SEED} after_schedule={schedule:?} result_value=ikis:{ikis:?}"
    );

    assert!(schedule.is_empty());
    assert!(ikis.is_empty());
    assert_eq!(total_wall_ms(&ikis), 0);
}

#[test]
fn single_character_first_iki_is_zero() {
    let dynamics = natural_fast();
    let before = "H";
    let schedule = sample_typing_schedule(before, &dynamics, Some(SEED));
    let ikis = ikis_ms(&schedule);
    println!(
        "readback=dynamics_natural_hello_world edge=single_char before=text:{before:?},seed:{SEED} after_schedule={schedule:?} result_value=ikis:{ikis:?}"
    );

    assert_eq!(ikis, [0]);
    assert_eq!(total_wall_ms(&ikis), 0);
}

#[test]
fn seed_42_is_deterministic_and_seed_43_differs() {
    let dynamics = natural_fast();
    let first = sample_typing_schedule(TEXT, &dynamics, Some(SEED));
    let second = sample_typing_schedule(TEXT, &dynamics, Some(SEED));
    let different = sample_typing_schedule(TEXT, &dynamics, Some(43));
    let first_ikis = ikis_ms(&first);
    let different_ikis = ikis_ms(&different);
    println!(
        "readback=dynamics_natural_hello_world edge=seed_determinism before=text:{TEXT:?},seed:{SEED} after_seed_42={first_ikis:?} after_seed_43={different_ikis:?} result_value=same_seed_equal:{},different_seed_differs:{}",
        first == second,
        first_ikis != different_ikis
    );

    assert_eq!(first, second);
    assert_ne!(first_ikis, different_ikis);
}

const fn natural_fast() -> KeystrokeDynamics {
    KeystrokeDynamics::Natural {
        params: KeystrokeNaturalParams::FAST,
    }
}

fn ikis_ms(schedule: &[KeystrokeEvent]) -> Vec<u32> {
    schedule.iter().map(|event| event.iki_ms_before).collect()
}

fn total_wall_ms(ikis: &[u32]) -> u32 {
    ikis.iter().copied().sum()
}

fn snapshot_text(ikis: &[u32], total_wall_ms: u32) -> String {
    format!(
        "text: {TEXT:?}\nseed: {SEED}\ndynamics: Natural::FAST\ntotal_wall_ms: {total_wall_ms}\nikis_ms: {ikis:?}\n"
    )
}
