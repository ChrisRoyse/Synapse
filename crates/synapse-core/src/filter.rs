use serde_json::Value;

use crate::{DataPredicate, Event, EventFilter};

#[must_use]
pub fn matches_event_filter(filter: &EventFilter, event: &Event) -> bool {
    match filter {
        EventFilter::All => true,
        EventFilter::None => false,
        EventFilter::Kind { kind } => event.kind == *kind,
        EventFilter::Source { source } => event.source == *source,
        EventFilter::And { args } => args.iter().all(|item| matches_event_filter(item, event)),
        EventFilter::Or { args } => args.iter().any(|item| matches_event_filter(item, event)),
        EventFilter::Not { arg } => !matches_event_filter(arg, event),
        EventFilter::Data { path, predicate } => {
            matches_data_predicate(predicate, event.data.pointer(path))
        }
    }
}

#[must_use]
pub fn matches_data_predicate(predicate: &DataPredicate, value: Option<&Value>) -> bool {
    match predicate {
        DataPredicate::Exists => value.is_some(),
        DataPredicate::Eq { value: expected } => value == Some(expected),
        DataPredicate::Ne { value: expected } => value.is_some_and(|actual| actual != expected),
        DataPredicate::Lt { value: expected } => {
            compare_values(value, expected).is_some_and(std::cmp::Ordering::is_lt)
        }
        DataPredicate::Le { value: expected } => {
            compare_values(value, expected).is_some_and(std::cmp::Ordering::is_le)
        }
        DataPredicate::Gt { value: expected } => {
            compare_values(value, expected).is_some_and(std::cmp::Ordering::is_gt)
        }
        DataPredicate::Ge { value: expected } => {
            compare_values(value, expected).is_some_and(std::cmp::Ordering::is_ge)
        }
        DataPredicate::Regex { pattern } => value.and_then(Value::as_str).is_some_and(|actual| {
            regex::Regex::new(pattern).is_ok_and(|regex| regex.is_match(actual))
        }),
        DataPredicate::InSet { values } => {
            value.is_some_and(|actual| values.iter().any(|item| item == actual))
        }
    }
}

fn compare_values(value: Option<&Value>, expected: &Value) -> Option<std::cmp::Ordering> {
    let actual = value?;

    match (actual, expected) {
        (Value::Number(actual), Value::Number(expected)) => {
            actual.as_f64()?.partial_cmp(&expected.as_f64()?)
        }
        (Value::String(actual), Value::String(expected)) => Some(actual.cmp(expected)),
        _ => None,
    }
}
