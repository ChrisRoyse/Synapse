//! Enrich `#[serde(deny_unknown_fields)]` param-deserialize failures into
//! actionable, self-correcting errors (#1593).
//!
//! Every facade param struct is `deny_unknown_fields` (fail-closed by design),
//! but rmcp surfaces the raw serde message `failed to deserialize parameters:
//! unknown field \`X\`, expected one of \`a\`, \`b\`` — which lists the accepted
//! fields but never tells the agent where the field it passed actually belongs.
//! Telemetry showed three recurring dead-ends: camelCase instead of snake_case
//! (`awaitPromise` -> `await_promise`), a nested-spec field hoisted to the
//! envelope top level (`timeout_ms` on `browser_wait` -> `wait.<condition>.
//! timeout_ms`), and an operation *value* passed as a field (`set` on `profile`,
//! which is a value of `operation`).
//!
//! This module does NOT weaken the contract — the call still errors. It only
//! rewrites the opaque serde message into a richer, structured error that names
//! the likely correct location. The mechanism is deliberately generic: it parses
//! serde's own `expected one of ...` field list out of the message and matches
//! the offending field against it (case-fold + camelCase→snake_case + bounded
//! Levenshtein), so any typo on any facade is caught without a hand-maintained
//! per-field list. A small [`registry_hint`] handles the two mistake classes a
//! flat field list cannot express: hoisted nested fields and
//! operation-value-as-field.

use rmcp::model::{ErrorCode, ErrorData};
use serde_json::json;
use synapse_core::error_codes;

/// rmcp wraps every `Parameters<T>` deserialize failure with this prefix
/// (`rmcp::handler::server::tool`). We only enrich errors that carry it so we
/// never touch domain errors that happen to share the INVALID_PARAMS code.
const DESERIALIZE_PREFIX: &str = "failed to deserialize parameters:";

/// Parsed shape of serde's `unknown field` error: the offending field plus the
/// list of accepted fields serde already emitted. Both are borrowed from the
/// raw message.
struct UnknownField<'a> {
    field: &'a str,
    accepted: Vec<&'a str>,
}

/// A concrete, actionable correction for an unknown field.
enum Hint {
    /// The field is a near-miss for a real accepted field (typo / camelCase).
    DidYouMean { candidate: String },
    /// The field was hoisted to the wrong nesting level; it belongs elsewhere.
    NestedLocation { location: String },
    /// The field is actually a value of a discriminator enum, not a field.
    OperationValue {
        operation_field: String,
        note: String,
    },
}

/// Enrich a raw rmcp param-deserialize error into a structured, actionable
/// [`ErrorData`]. Returns `None` when the message is not an enrichable
/// `unknown field` deserialize failure (e.g. a type mismatch or missing field),
/// letting the caller keep its existing normalization.
pub(super) fn enrich_param_deserialize_error(tool: &str, raw_message: &str) -> Option<ErrorData> {
    let inner = raw_message.strip_prefix(DESERIALIZE_PREFIX)?.trim();
    let parsed = parse_unknown_field(inner)?;
    let hint =
        registry_hint(tool, parsed.field).or_else(|| generic_hint(parsed.field, &parsed.accepted));

    let (message, remediation, mut data) = match &hint {
        Some(Hint::DidYouMean { candidate }) => (
            format!(
                "{tool}: unknown field `{}` — did you mean `{candidate}`? (facade fields are snake_case and fail-closed)",
                parsed.field
            ),
            format!("rename `{}` to `{candidate}`", parsed.field),
            json!({ "hint_kind": "did_you_mean", "did_you_mean": candidate }),
        ),
        Some(Hint::NestedLocation { location }) => (
            format!(
                "{tool}: unknown field `{}` at the envelope top level — it belongs on {location}",
                parsed.field
            ),
            format!("move `{}` to {location}", parsed.field),
            json!({ "hint_kind": "nested_location", "belongs_at": location }),
        ),
        Some(Hint::OperationValue {
            operation_field,
            note,
        }) => (
            format!(
                "{tool}: unknown field `{}` — `{}` is a value of `{operation_field}`, not a field. {note}",
                parsed.field, parsed.field
            ),
            format!(
                "pass {operation_field}=\"{}\" instead of a `{}` field",
                parsed.field, parsed.field
            ),
            json!({ "hint_kind": "operation_value", "operation_field": operation_field }),
        ),
        None => (
            format!(
                "{tool}: unknown field `{}`; accepted fields are {}",
                parsed.field,
                format_accepted(&parsed.accepted)
            ),
            format!(
                "pass only accepted fields: {}",
                format_accepted(&parsed.accepted)
            ),
            json!({ "hint_kind": "no_match" }),
        ),
    };

    if let Some(object) = data.as_object_mut() {
        object.insert("code".to_owned(), json!(error_codes::TOOL_PARAMS_INVALID));
        object.insert("tool".to_owned(), json!(tool));
        object.insert("unknown_field".to_owned(), json!(parsed.field));
        object.insert("accepted_fields".to_owned(), json!(parsed.accepted));
        object.insert("remediation".to_owned(), json!(remediation));
        object.insert(
            "source_of_truth".to_owned(),
            json!(
                "serde deny_unknown_fields on the typed facade params (fail-closed); this hint is advisory"
            ),
        );
    }

    Some(ErrorData::new(ErrorCode(-32099), message, Some(data)))
}

/// Parse serde's `unknown field \`X\`, expected one of \`a\`, \`b\`` message.
///
/// serde formats the accepted set with arity-dependent phrasing (`there are no
/// fields`, `expected \`a\``, `expected \`a\` or \`b\``, `expected one of \`a\`,
/// \`b\`, \`c\``). Rather than match each phrasing we extract every
/// backtick-delimited token: the first is the offending field and the rest are
/// the accepted fields. This is robust to all arities and to nested failures
/// (where the accepted list is the nested struct's fields).
fn parse_unknown_field(message: &str) -> Option<UnknownField<'_>> {
    let rest = message.strip_prefix("unknown field ")?;
    let mut tokens = Vec::new();
    let mut remaining = rest;
    while let Some(open) = remaining.find('`') {
        let after = &remaining[open + 1..];
        let close = after.find('`')?;
        tokens.push(&after[..close]);
        remaining = &after[close + 1..];
    }
    let (field, accepted) = tokens.split_first()?;
    Some(UnknownField {
        field,
        accepted: accepted.to_vec(),
    })
}

/// Per-facade corrections for mistakes a flat accepted-field list cannot express:
/// nested fields hoisted to the envelope, and operation *values* passed as
/// fields. Kept deliberately small and anchored to telemetry-verified mistakes;
/// generic typo matching in [`generic_hint`] covers the long tail.
fn registry_hint(tool: &str, field: &str) -> Option<Hint> {
    match (tool, field) {
        // `browser_wait` accepts only `operation`, `wait`, and (since #1593) the
        // top-level `cdp_target_id`/`window_hwnd` target aliases. The polling
        // budget fields live on the nested condition spec, not the envelope.
        ("browser_wait", "timeout_ms" | "polling_interval_ms" | "interval_ms") => {
            Some(Hint::NestedLocation {
                location: format!(
                    "the nested condition spec: wait.<condition>.{field} (e.g. wait.text.{field})"
                ),
            })
        }
        // `profile` operation values passed as fields. `ProfileOperation` values
        // are status/set/grant_reality_write/revoke_reality_write.
        ("profile", "status" | "set" | "grant_reality_write" | "revoke_reality_write") => {
            Some(Hint::OperationValue {
                operation_field: "operation".to_owned(),
                note: format!(
                    "pass the `{field}` operation's own fields (e.g. profile, reason, confirm_break_glass) at the top level"
                ),
            })
        }
        // `act` resolves its target from the session binding, not a per-call
        // field, so there is no envelope target alias to fold into.
        ("act", "cdp_target_id" | "window_hwnd") => Some(Hint::NestedLocation {
            location:
                "the session target binding: set it first with `set_target` (or `target` operation=set); `act` has no per-call target field"
                    .to_owned(),
        }),
        _ => None,
    }
}

/// Generic near-miss matching against serde's accepted-field list, in order of
/// confidence: exact camelCase→snake_case, case/underscore-insensitive equality,
/// then bounded Levenshtein distance.
fn generic_hint(field: &str, accepted: &[&str]) -> Option<Hint> {
    if accepted.is_empty() {
        return None;
    }
    let snake = to_snake_case(field);
    if let Some(hit) = accepted.iter().find(|candidate| **candidate == snake) {
        return Some(Hint::DidYouMean {
            candidate: (*hit).to_owned(),
        });
    }
    let folded = fold_key(field);
    if let Some(hit) = accepted
        .iter()
        .find(|candidate| fold_key(candidate) == folded)
    {
        return Some(Hint::DidYouMean {
            candidate: (*hit).to_owned(),
        });
    }
    let lower = field.to_ascii_lowercase();
    let mut best: Option<(usize, &str)> = None;
    for candidate in accepted {
        let distance = levenshtein(&lower, &candidate.to_ascii_lowercase());
        if best.is_none_or(|(best_distance, _)| distance < best_distance) {
            best = Some((distance, candidate));
        }
    }
    let (distance, candidate) = best?;
    let threshold = (field.chars().count() / 3).max(2);
    (distance <= threshold).then(|| Hint::DidYouMean {
        candidate: candidate.to_owned(),
    })
}

/// Case- and underscore-insensitive key used to detect camelCase/case-only
/// variants: `awaitPromise`, `await_promise`, and `AwaitPromise` all fold equal.
fn fold_key(value: &str) -> String {
    value
        .chars()
        .filter(|character| *character != '_')
        .flat_map(char::to_lowercase)
        .collect()
}

/// Convert `camelCase`/`PascalCase` to `snake_case`. Runs of uppercase and
/// digits are handled conservatively; already-snake input is returned unchanged.
fn to_snake_case(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 4);
    for (index, character) in value.char_indices() {
        if character.is_ascii_uppercase() {
            if index != 0 && !out.ends_with('_') {
                out.push('_');
            }
            out.push(character.to_ascii_lowercase());
        } else {
            out.push(character);
        }
    }
    out
}

/// Standard Levenshtein edit distance (two-row DP) over char sequences.
fn levenshtein(left: &str, right: &str) -> usize {
    let right_chars: Vec<char> = right.chars().collect();
    let mut previous: Vec<usize> = (0..=right_chars.len()).collect();
    let mut current = vec![0usize; right_chars.len() + 1];
    for (row, left_char) in left.chars().enumerate() {
        current[0] = row + 1;
        for (column, right_char) in right_chars.iter().enumerate() {
            let cost = usize::from(left_char != *right_char);
            current[column + 1] = (previous[column + 1] + 1)
                .min(current[column] + 1)
                .min(previous[column] + cost);
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[right_chars.len()]
}

fn format_accepted(accepted: &[&str]) -> String {
    accepted
        .iter()
        .map(|field| format!("`{field}`"))
        .collect::<Vec<_>>()
        .join(", ")
}
