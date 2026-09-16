use chrono::Utc;
use serde_json::json;
use synapse_core::{Event, EventExtension, EventRef, EventSource};

use crate::{PerceptionError, PerceptionResult};

/// Validates one profile event extension before it can emit derived events.
///
/// # Errors
///
/// Returns [`PerceptionError::EventExtensionInvalid`] when the extension has an
/// empty name, empty emitted kind, an invalid filter tree, or a filter that is
/// syntactically always true.
pub fn validate_event_extension(extension: &EventExtension) -> PerceptionResult<()> {
    if extension.name.trim().is_empty() {
        return Err(PerceptionError::EventExtensionInvalid {
            name: extension.name.clone(),
            detail: "name must not be empty".to_owned(),
        });
    }
    if extension.emits_kind.trim().is_empty() {
        return Err(PerceptionError::EventExtensionInvalid {
            name: extension.name.clone(),
            detail: "emits_kind must not be empty".to_owned(),
        });
    }
    extension
        .from_filter
        .validate()
        .map_err(|error| PerceptionError::EventExtensionInvalid {
            name: extension.name.clone(),
            detail: format!("filter invalid: {error}"),
        })?;
    if extension.from_filter.is_trivially_always_true() {
        return Err(PerceptionError::EventExtensionInvalid {
            name: extension.name.clone(),
            detail: "from_filter must not be trivially always true".to_owned(),
        });
    }
    Ok(())
}

/// Validates every profile event extension in registration order.
///
/// # Errors
///
/// Returns [`PerceptionError::EventExtensionInvalid`] for the first invalid
/// extension.
pub fn validate_event_extensions(extensions: &[EventExtension]) -> PerceptionResult<()> {
    for extension in extensions {
        validate_event_extension(extension)?;
    }
    Ok(())
}

/// Evaluates profile event extensions against a real event and returns derived events.
///
/// The returned events keep their source as [`EventSource::Perception`] and
/// carry a correlation back to the triggering event sequence.
///
/// # Errors
///
/// Returns [`PerceptionError::EventExtensionInvalid`] when an extension is
/// invalid or when sequence assignment would overflow.
pub fn evaluate_event_extensions(
    extensions: &[EventExtension],
    trigger: &Event,
    first_seq: u64,
) -> PerceptionResult<Vec<Event>> {
    validate_event_extensions(extensions)?;

    let mut events = Vec::new();
    for extension in extensions {
        if !extension.from_filter.matches(trigger) {
            continue;
        }
        let seq = first_seq
            .checked_add(u64::try_from(events.len()).map_err(|error| {
                PerceptionError::EventExtensionInvalid {
                    name: extension.name.clone(),
                    detail: format!("event count overflow: {error}"),
                }
            })?)
            .ok_or_else(|| PerceptionError::EventExtensionInvalid {
                name: extension.name.clone(),
                detail: "sequence assignment overflowed".to_owned(),
            })?;
        events.push(Event {
            seq,
            at: Utc::now(),
            source: EventSource::Perception,
            kind: extension.emits_kind.clone(),
            data: json!({
                "extension_name": extension.name.clone(),
                "trigger_seq": trigger.seq,
                "trigger_source": trigger.source,
                "trigger_kind": trigger.kind.clone(),
                "trigger_data": trigger.data.clone(),
            }),
            correlations: vec![EventRef {
                seq: trigger.seq,
                relation: "event_extension_trigger".to_owned(),
            }],
        });
    }
    Ok(events)
}
