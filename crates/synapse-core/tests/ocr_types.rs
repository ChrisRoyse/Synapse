#![allow(clippy::missing_const_for_fn)]

use std::fmt::Debug;

use proptest::{
    prelude::*,
    test_runner::{Config, TestRng, TestRunner},
};
use serde::{Serialize, de::DeserializeOwned};
use synapse_core::{OcrConfidenceSource, OcrResult, OcrWord, Rect};

#[test]
fn ocr_type_edge_round_trips_with_readback() -> Result<(), Box<dyn std::error::Error>> {
    round_trip("OcrResult", "empty", empty_ocr_result())?;
    round_trip("OcrResult", "single_word", single_word_ocr_result())?;
    round_trip("OcrResult", "fully_populated", full_ocr_result())?;

    round_trip("OcrWord", "empty", empty_ocr_word())?;
    round_trip("OcrWord", "required_only", required_ocr_word())?;
    round_trip("OcrWord", "fully_populated", full_ocr_word("World", 80, 16))?;

    Ok(())
}

#[test]
fn ocr_type_json_snapshots() -> Result<(), Box<dyn std::error::Error>> {
    insta::assert_json_snapshot!(
        "ocr_result_empty_round_trip",
        round_trip("OcrResult", "snapshot_empty", empty_ocr_result())?
    );
    insta::assert_json_snapshot!(
        "ocr_result_full_round_trip",
        round_trip("OcrResult", "snapshot_full", full_ocr_result())?
    );
    insta::assert_json_snapshot!(
        "ocr_word_round_trip",
        round_trip("OcrWord", "snapshot", full_ocr_word("Synapse", 12, 4))?
    );
    Ok(())
}

#[test]
fn ocr_types_reject_unknown_fields() -> Result<(), Box<dyn std::error::Error>> {
    reject_unknown_field("OcrResult", full_ocr_result())?;
    reject_unknown_field("OcrWord", full_ocr_word("Synapse", 12, 4))?;
    Ok(())
}

#[test]
fn ocr_types_proptest_json_round_trip_is_deterministic() -> Result<(), Box<dyn std::error::Error>> {
    assert_strategy_round_trips("OcrResult", ocr_result_strategy())?;
    assert_strategy_round_trips("OcrWord", ocr_word_strategy())?;
    Ok(())
}

#[allow(clippy::needless_pass_by_value)]
fn round_trip<T>(type_name: &str, edge: &str, value: T) -> Result<T, Box<dyn std::error::Error>>
where
    T: Clone + Debug + PartialEq + Serialize + DeserializeOwned + 'static,
{
    let before = serde_json::to_value(value.clone())?;
    println!("readback=json_ocr_type type={type_name} edge={edge} before={before}");
    let parsed = serde_json::from_value::<T>(before)?;
    let after = serde_json::to_value(&parsed)?;
    println!(
        "readback=json_ocr_type type={type_name} edge={edge} after={after} result_value={after}"
    );
    assert_eq!(parsed, value);
    Ok(parsed)
}

fn reject_unknown_field<T>(type_name: &str, value: T) -> Result<(), Box<dyn std::error::Error>>
where
    T: Clone + Debug + PartialEq + Serialize + DeserializeOwned + 'static,
{
    let mut json = serde_json::to_value(value)?;
    println!("readback=json_ocr_type_unknown type={type_name} before={json}");
    let serde_json::Value::Object(ref mut map) = json else {
        panic!("{type_name} should serialize to an object");
    };
    map.insert("unknown_field".to_owned(), serde_json::json!(true));
    let Err(err) = serde_json::from_value::<T>(json.clone()) else {
        panic!("unknown field should reject");
    };
    println!("readback=json_ocr_type_unknown type={type_name} after={err}");
    Ok(())
}

fn assert_strategy_round_trips<T, S>(
    type_name: &str,
    strategy: S,
) -> Result<(), Box<dyn std::error::Error>>
where
    T: Clone + Debug + PartialEq + Serialize + DeserializeOwned + 'static,
    S: Strategy<Value = T>,
{
    let config = Config {
        cases: 1_000,
        failure_persistence: None,
        ..Config::default()
    };
    let algorithm = config.rng_algorithm;
    let mut runner = TestRunner::new_with_rng(config, TestRng::deterministic_rng(algorithm));

    println!("readback=json_ocr_type_proptest type={type_name} before=cases:1000");
    runner.run(&strategy, |value| {
        let json = serde_json::to_value(value.clone())?;
        let parsed = serde_json::from_value::<T>(json)?;
        prop_assert_eq!(parsed, value);
        Ok(())
    })?;
    println!(
        "readback=json_ocr_type_proptest type={type_name} after=cases:1000 result_value=all_round_tripped"
    );
    Ok(())
}

fn empty_ocr_result() -> OcrResult {
    OcrResult {
        full_text: String::new(),
        words: Vec::new(),
        confidence: 0.0,
        confidence_source: OcrConfidenceSource::Unsupported,
        no_text: false,
        region: rect(0, 0, 0, 0),
        lang: "und".to_owned(),
        perceived_text_notice: None,
        suspected_injection: Vec::new(),
    }
}

fn single_word_ocr_result() -> OcrResult {
    OcrResult {
        full_text: "Synapse".to_owned(),
        words: vec![full_ocr_word("Synapse", 12, 4)],
        confidence: 0.99,
        confidence_source: OcrConfidenceSource::Engine,
        no_text: false,
        region: rect(5, 7, 256, 64),
        lang: "en".to_owned(),
        perceived_text_notice: None,
        suspected_injection: Vec::new(),
    }
}

fn full_ocr_result() -> OcrResult {
    OcrResult {
        full_text: "Synapse Ready".to_owned(),
        words: vec![
            full_ocr_word("Synapse", 12, 4),
            full_ocr_word("Ready", 92, 4),
        ],
        confidence: 0.935,
        confidence_source: OcrConfidenceSource::Engine,
        no_text: false,
        region: rect(5, 7, 256, 64),
        lang: "en-US".to_owned(),
        perceived_text_notice: None,
        suspected_injection: Vec::new(),
    }
}

fn empty_ocr_word() -> OcrWord {
    OcrWord {
        text: String::new(),
        bbox: rect(0, 0, 0, 0),
        confidence: 0.0,
        confidence_source: OcrConfidenceSource::Unsupported,
    }
}

fn required_ocr_word() -> OcrWord {
    OcrWord {
        text: "A".to_owned(),
        bbox: rect(1, 2, 3, 4),
        confidence: 1.0,
        confidence_source: OcrConfidenceSource::Engine,
    }
}

fn full_ocr_word(text: &str, x: i32, y: i32) -> OcrWord {
    OcrWord {
        text: text.to_owned(),
        bbox: rect(x, y, 72, 18),
        confidence: if text == "Ready" { 0.88 } else { 0.99 },
        confidence_source: OcrConfidenceSource::Engine,
    }
}

const fn rect(x: i32, y: i32, w: i32, h: i32) -> Rect {
    Rect { x, y, w, h }
}

fn ocr_result_strategy() -> impl Strategy<Value = OcrResult> {
    (
        text_strategy(48),
        prop::collection::vec(ocr_word_strategy(), 0..=8),
        confidence_strategy(),
        rect_strategy(),
        lang_strategy(),
    )
        .prop_map(|(full_text, words, confidence, region, lang)| OcrResult {
            full_text,
            words,
            confidence,
            confidence_source: OcrConfidenceSource::Engine,
            region,
            lang,
            no_text: false,
            perceived_text_notice: None,
            suspected_injection: Vec::new(),
        })
}

fn ocr_word_strategy() -> impl Strategy<Value = OcrWord> {
    (text_strategy(16), rect_strategy(), confidence_strategy()).prop_map(
        |(text, bbox, confidence)| OcrWord {
            text,
            bbox,
            confidence,
            confidence_source: OcrConfidenceSource::Engine,
        },
    )
}

fn rect_strategy() -> impl Strategy<Value = Rect> {
    (-4096i32..=4096, -4096i32..=4096, 0i32..=4096, 0i32..=4096).prop_map(|(x, y, w, h)| Rect {
        x,
        y,
        w,
        h,
    })
}

fn confidence_strategy() -> impl Strategy<Value = f32> {
    (0u16..=1_000).prop_map(|value| f32::from(value) / 1_000.0)
}

fn lang_strategy() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("und".to_owned()),
        Just("en".to_owned()),
        Just("en-US".to_owned()),
        Just("ja-JP".to_owned()),
    ]
}

fn text_strategy(max_len: usize) -> impl Strategy<Value = String> {
    prop::collection::vec(
        prop_oneof![
            Just(' '),
            prop::char::range('0', '9'),
            prop::char::range('A', 'Z'),
            prop::char::range('a', 'z'),
        ],
        0..=max_len,
    )
    .prop_map(|chars| chars.into_iter().collect())
}
