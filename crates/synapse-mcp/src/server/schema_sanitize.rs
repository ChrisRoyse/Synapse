//! Tool-schema sanitization for MCP `tools/list` output.
//!
//! `schemars` renders arbitrary-JSON fields (`serde_json::Value`,
//! `Option<serde_json::Value>`) as the JSON Schema boolean `true`. That is a
//! technically valid schema (draft-06+: a boolean schema, where `true` means
//! "any value", equivalent to `{}`), but strict MCP clients — including the
//! Zod-based validator in the official clients — reject a property whose schema
//! is a bare boolean and fail the entire `tools/list` response with
//! `Invalid input`. The symptom is "Reconnected to <server>, but fetching tools
//! failed: tools[..].(input|output)Schema.properties.<field> Invalid input",
//! after which none of the server's tools are usable.
//!
//! See the upstream discussion of this exact incompatibility:
//! <https://github.com/PrefectHQ/fastmcp/issues/3783> (boolean property schemas)
//! and schemars' documented behaviour for `serde_json::Value`.
//!
//! Rather than annotate every `Value` field individually (fragile — the next
//! `Value` field reintroduces the bug), we normalize at the serving boundary:
//! every emitted tool schema is walked and any boolean found in a *schema
//! position that strict clients validate as a subschema* is replaced with an
//! explicit, fully permissive object schema. This is exhaustive over current and
//! future tools and is enforced by `schema_sanitize_tests`.
//!
//! Booleans in `additionalProperties` / `additionalItems` / `unevaluated*`
//! positions are intentionally preserved: a boolean there is meaningful and is
//! accepted by clients.
//!
//! ## Non-standard `format` annotations
//!
//! `schemars` renders Rust numeric types with OpenAPI-style `format`
//! annotations that are **not** part of the JSON Schema format registry:
//! `u32` → `"uint32"`, `usize` → `"uint"`, `u64` → `"uint64"`, `i32` →
//! `"int32"`, `f32` → `"float"`, and so on (see
//! <https://github.com/GREsau/schemars/issues/43>). The JSON Schema 2020-12 spec
//! says an implementation MUST NOT *fail* on an unknown `format`, so compliant
//! clients (Claude Code) only log `unknown format "uint32" ignored in schema …`
//! — but that floods client logs, and strict validators (Ajv in default mode,
//! <https://github.com/ajv-validator/ajv/issues/2021>) reject them outright.
//!
//! These annotations carry **zero** validation value here: `schemars` already
//! emits `"type": "integer"` (and `"minimum": 0` for unsigned) alongside them,
//! which fully constrains the value. So we normalize at the serving boundary by
//! removing every `format` whose value is not in the standard JSON Schema 2020-12
//! format registry (`STANDARD_JSON_SCHEMA_FORMATS`). Standard formats a client
//! understands (`uuid`, `date-time`, `email`, …) are preserved. This is an
//! allowlist, not a denylist, so a new numeric field — or any future
//! non-standard format — can never reintroduce the warning. Enforced by
//! `real_tool_schemas_have_no_nonstandard_formats_after_sanitize`.

use std::{borrow::Cow, sync::Arc};

use rmcp::model::Tool;
use serde_json::{Map, Value};

/// Keywords whose value is a *map* of subschemas. A boolean value of any member
/// is the client-rejected case and is rewritten.
const SCHEMA_MAP_KEYWORDS: &[&str] = &[
    "$defs",
    "definitions",
    "dependentSchemas",
    "patternProperties",
    "properties",
];

/// Keywords whose value is an *array* of subschemas. A boolean element is
/// rewritten.
const SCHEMA_ARRAY_KEYWORDS: &[&str] = &["oneOf", "anyOf", "allOf", "prefixItems"];

/// Keywords whose value is a single subschema. A boolean value is rewritten.
const SCHEMA_VALUE_KEYWORDS: &[&str] = &[
    "contains",
    "else",
    "if",
    "items",
    "not",
    "propertyNames",
    "then",
];

/// The complete set of `format` values defined by the JSON Schema 2020-12
/// format-annotation vocabulary. Any `format` whose value is **not** in this
/// allowlist is a non-standard annotation (e.g. schemars' `uint32`/`int64`/
/// `float`) and is stripped by [`strip_nonstandard_format`]. Kept sorted so a
/// binary search is valid and the list is easy to audit against the spec.
const STANDARD_JSON_SCHEMA_FORMATS: &[&str] = &[
    "date",
    "date-time",
    "duration",
    "email",
    "hostname",
    "idn-email",
    "idn-hostname",
    "ipv4",
    "ipv6",
    "iri",
    "iri-reference",
    "json-pointer",
    "regex",
    "relative-json-pointer",
    "time",
    "uri",
    "uri-reference",
    "uri-template",
    "uuid",
];

/// JSON Schema annotation fields that are fed directly to MCP clients and LLMs.
/// Keep these ASCII-only so Windows console/code-page boundaries cannot turn
/// valid UTF-8 punctuation into mojibake in `tools/list`.
const TEXT_ANNOTATION_KEYS: &[&str] = &["$comment", "description", "title"];

/// True if `format` is a standard JSON Schema 2020-12 format that compliant MCP
/// clients recognize and therefore must be preserved.
fn is_standard_format(format: &str) -> bool {
    STANDARD_JSON_SCHEMA_FORMATS.binary_search(&format).is_ok()
}

fn normalize_metadata_text(input: &str) -> Cow<'_, str> {
    if input.is_ascii() {
        return Cow::Borrowed(input);
    }

    let mut output = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '\u{00a0}' => output.push(' '),
            '\u{00b1}' => output.push_str("+/-"),
            '\u{00d7}' => output.push('x'),
            '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2212}' => {
                output.push('-');
            }
            '\u{2014}' => output.push('-'),
            '\u{2018}' | '\u{2019}' => output.push('\''),
            '\u{201c}' | '\u{201d}' => output.push('"'),
            '\u{2026}' => output.push_str("..."),
            '\u{2190}' => output.push_str("<-"),
            '\u{2192}' => output.push_str("->"),
            '\u{21d2}' => output.push_str("=>"),
            '\u{2208}' => output.push_str("in"),
            '\u{2209}' => output.push_str("not in"),
            '\u{03a3}' | '\u{2211}' => output.push_str("sum"),
            '\u{2264}' => output.push_str("<="),
            '\u{2265}' => output.push_str(">="),
            '\u{2713}' => output.push_str("check"),
            _ => output.push(ch),
        }
    }

    Cow::Owned(output)
}

/// Removes a non-standard `format` annotation from a single schema object,
/// leaving the value's `type`/`minimum`/`maximum` constraints intact. Standard
/// formats are preserved. No-op when there is no `format` or it is standard.
fn strip_nonstandard_format(map: &mut Map<String, Value>) {
    let is_nonstandard = matches!(
        map.get("format"),
        Some(Value::String(format)) if !is_standard_format(format)
    );
    if is_nonstandard {
        map.remove("format");
    }
}

/// Sanitizes every tool's input and output schema so no property/composition
/// subschema is a bare boolean. Returns tools safe to send over `tools/list`.
#[must_use]
pub fn sanitize_tools(tools: Vec<Tool>) -> Vec<Tool> {
    tools.into_iter().map(sanitize_tool).collect()
}

fn sanitize_tool(mut tool: Tool) -> Tool {
    if let Some(description) = tool.description.as_ref() {
        if let Cow::Owned(normalized) = normalize_metadata_text(description.as_ref()) {
            tool.description = Some(Cow::Owned(normalized));
        }
    }
    tool.input_schema = sanitize_schema_object(&tool.input_schema);
    if let Some(output) = &tool.output_schema {
        tool.output_schema = Some(sanitize_schema_object(output));
    }
    tool
}

fn sanitize_schema_object(schema: &Arc<Map<String, Value>>) -> Arc<Map<String, Value>> {
    let mut cloned = (**schema).clone();
    rewrite_map(&mut cloned);
    Arc::new(cloned)
}

/// A fully permissive but explicit object schema used to replace a bare boolean
/// `true`. Every JSON value validates against it, and every strict client
/// accepts it because it is an object with a concrete `type` union.
fn permissive_schema() -> Value {
    Value::Object(Map::from_iter([(
        "type".to_owned(),
        Value::Array(vec![
            Value::String("object".to_owned()),
            Value::String("array".to_owned()),
            Value::String("string".to_owned()),
            Value::String("number".to_owned()),
            Value::String("boolean".to_owned()),
            Value::String("null".to_owned()),
        ]),
    )]))
}

/// A never-matching object schema used to replace a bare boolean `false`.
fn never_schema() -> Value {
    Value::Object(Map::from_iter([(
        "not".to_owned(),
        Value::Object(Map::new()),
    )]))
}

fn boolean_as_schema(b: bool) -> Value {
    if b {
        permissive_schema()
    } else {
        never_schema()
    }
}

fn rewrite_value(value: &mut Value) {
    match value {
        Value::Object(map) => rewrite_map(map),
        Value::Array(items) => {
            for item in items.iter_mut() {
                rewrite_value(item);
            }
        }
        _ => {}
    }
}

fn rewrite_schema_value(value: &mut Value) {
    match value {
        Value::Bool(b) => *value = boolean_as_schema(*b),
        Value::Object(map) => rewrite_map(map),
        Value::Array(items) => {
            for item in items.iter_mut() {
                rewrite_schema_value(item);
            }
        }
        _ => {}
    }
}

fn rewrite_map(map: &mut Map<String, Value>) {
    for (key, child) in map.iter_mut() {
        if TEXT_ANNOTATION_KEYS.contains(&key.as_str()) {
            if let Value::String(text) = child
                && let Cow::Owned(normalized) = normalize_metadata_text(text)
            {
                *text = normalized;
            }
        } else if SCHEMA_MAP_KEYWORDS.contains(&key.as_str()) {
            if let Value::Object(members) = child {
                for member in members.values_mut() {
                    rewrite_schema_value(member);
                }
            } else {
                rewrite_value(child);
            }
        } else if SCHEMA_ARRAY_KEYWORDS.contains(&key.as_str()) {
            if let Value::Array(elements) = child {
                for element in elements.iter_mut() {
                    rewrite_schema_value(element);
                }
            } else {
                rewrite_value(child);
            }
        } else if SCHEMA_VALUE_KEYWORDS.contains(&key.as_str()) {
            rewrite_schema_value(child);
        } else {
            // `additionalProperties` (object form), etc. Recurse to reach nested
            // schema keywords, but do not rewrite a boolean that legitimately
            // lives in
            // `additionalProperties`/`additionalItems`/`unevaluated*`.
            rewrite_value(child);
        }
    }
    // After recursing into children, drop any non-standard `format` annotation on
    // this schema object (e.g. schemars' `uint32`/`int64`/`float`). Done last so
    // nested schemas reached above are normalized by their own `rewrite_map`.
    strip_nonstandard_format(map);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    /// Returns every JSON-pointer-ish path at which a *boolean* appears in a
    /// client-validated schema position. These are exactly the positions strict
    /// MCP clients reject.
    fn bare_boolean_schema_paths(value: &Value, path: &str, out: &mut Vec<String>) {
        match value {
            Value::Object(map) => {
                for (key, child) in map {
                    let child_path = format!("{path}.{key}");
                    if SCHEMA_MAP_KEYWORDS.contains(&key.as_str()) {
                        if let Value::Object(members) = child {
                            for (mk, mv) in members {
                                if mv.is_boolean() {
                                    out.push(format!("{child_path}.{mk}"));
                                } else {
                                    bare_boolean_schema_paths(
                                        mv,
                                        &format!("{child_path}.{mk}"),
                                        out,
                                    );
                                }
                            }
                            continue;
                        }
                    } else if SCHEMA_ARRAY_KEYWORDS.contains(&key.as_str())
                        && let Value::Array(elements) = child
                    {
                        for (i, ev) in elements.iter().enumerate() {
                            if ev.is_boolean() {
                                out.push(format!("{child_path}[{i}]"));
                            } else {
                                bare_boolean_schema_paths(ev, &format!("{child_path}[{i}]"), out);
                            }
                        }
                        continue;
                    } else if SCHEMA_VALUE_KEYWORDS.contains(&key.as_str()) {
                        if child.is_boolean() {
                            out.push(child_path);
                        } else {
                            bare_boolean_schema_paths(child, &child_path, out);
                        }
                        continue;
                    }
                    bare_boolean_schema_paths(child, &child_path, out);
                }
            }
            Value::Array(items) => {
                for (i, item) in items.iter().enumerate() {
                    bare_boolean_schema_paths(item, &format!("{path}[{i}]"), out);
                }
            }
            _ => {}
        }
    }

    fn text_metadata_issue(text: &str) -> Option<String> {
        if text.contains('â')
            || text.contains('Â')
            || text.contains('\u{fffd}')
            || text.contains("\\u00e2")
            || text.contains("\\u00c2")
            || text
                .chars()
                .any(|ch| ('\u{0080}'..='\u{009f}').contains(&ch))
        {
            return Some("mojibake sentinel".to_owned());
        }
        if let Some(ch) = text.chars().find(|ch| !ch.is_ascii()) {
            return Some(format!("non-ascii U+{:04X}", ch as u32));
        }
        None
    }

    fn text_metadata_issues(value: &Value, path: &str, out: &mut Vec<String>) {
        match value {
            Value::Object(map) => {
                for (key, child) in map {
                    let child_path = format!("{path}.{key}");
                    if TEXT_ANNOTATION_KEYS.contains(&key.as_str())
                        && let Value::String(text) = child
                        && let Some(issue) = text_metadata_issue(text)
                    {
                        out.push(format!("{child_path}: {issue}: {text:?}"));
                    }
                    text_metadata_issues(child, &child_path, out);
                }
            }
            Value::Array(items) => {
                for (i, item) in items.iter().enumerate() {
                    text_metadata_issues(item, &format!("{path}[{i}]"), out);
                }
            }
            _ => {}
        }
    }

    /// Full real tool surface, sanitized, must contain ZERO bare-boolean schema
    /// positions. This is the regression gate that keeps any current or future
    /// `serde_json::Value` tool field from breaking strict MCP clients.
    #[test]
    fn real_tool_schemas_have_no_bare_boolean_property_schemas_after_sanitize() {
        let tools = sanitize_tools(super::super::SynapseService::tool_router().list_all());
        let mut offenders = Vec::new();
        for tool in &tools {
            let input = Value::Object((*tool.input_schema).clone());
            bare_boolean_schema_paths(
                &input,
                &format!("{}.inputSchema", tool.name),
                &mut offenders,
            );
            if let Some(output) = &tool.output_schema {
                let output = Value::Object((**output).clone());
                bare_boolean_schema_paths(
                    &output,
                    &format!("{}.outputSchema", tool.name),
                    &mut offenders,
                );
            }
        }
        assert!(
            offenders.is_empty(),
            "sanitized tool schemas still contain bare boolean schemas (strict MCP clients reject these): {offenders:#?}"
        );
    }

    #[test]
    fn real_tool_metadata_text_has_no_mojibake_or_non_ascii_after_sanitize() {
        let tools = sanitize_tools(super::super::SynapseService::tool_router().list_all());
        let mut offenders = Vec::new();
        for tool in &tools {
            if let Some(description) = tool.description.as_ref()
                && let Some(issue) = text_metadata_issue(description.as_ref())
            {
                offenders.push(format!(
                    "{}.description: {issue}: {:?}",
                    tool.name,
                    description.as_ref()
                ));
            }

            let input = Value::Object((*tool.input_schema).clone());
            text_metadata_issues(
                &input,
                &format!("{}.inputSchema", tool.name),
                &mut offenders,
            );
            if let Some(output) = &tool.output_schema {
                let output = Value::Object((**output).clone());
                text_metadata_issues(
                    &output,
                    &format!("{}.outputSchema", tool.name),
                    &mut offenders,
                );
            }
        }
        assert!(
            offenders.is_empty(),
            "sanitized tool metadata still contains mojibake sentinel or non-ASCII annotation text: {offenders:#?}"
        );
    }

    /// The raw (un-sanitized) surface is expected to contain bare booleans
    /// (schemars emits them for `serde_json::Value`). If this ever becomes
    /// empty the sanitizer is no longer load-bearing, which is fine — but we
    /// assert it here so the gate above can never pass vacuously.
    #[test]
    fn raw_tool_schemas_do_contain_bare_booleans() {
        let tools = super::super::SynapseService::tool_router().list_all();
        let mut offenders = Vec::new();
        for tool in &tools {
            let input = Value::Object((*tool.input_schema).clone());
            bare_boolean_schema_paths(&input, "in", &mut offenders);
            if let Some(output) = &tool.output_schema {
                let output = Value::Object((**output).clone());
                bare_boolean_schema_paths(&output, "out", &mut offenders);
            }
        }
        assert!(
            !offenders.is_empty(),
            "expected schemars to emit at least one bare boolean schema for a serde_json::Value field"
        );
    }

    /// Collects every `(json-pointer-ish path, format)` pair where a schema
    /// object carries a non-standard `format` annotation (one not in
    /// [`STANDARD_JSON_SCHEMA_FORMATS`]). These are exactly the annotations that
    /// make strict MCP clients log `unknown format "uint32" ignored`.
    fn nonstandard_format_paths(value: &Value, path: &str, out: &mut Vec<(String, String)>) {
        match value {
            Value::Object(map) => {
                if let Some(Value::String(format)) = map.get("format")
                    && !is_standard_format(format)
                {
                    out.push((path.to_owned(), format.clone()));
                }
                for (key, child) in map {
                    nonstandard_format_paths(child, &format!("{path}.{key}"), out);
                }
            }
            Value::Array(items) => {
                for (i, item) in items.iter().enumerate() {
                    nonstandard_format_paths(item, &format!("{path}[{i}]"), out);
                }
            }
            _ => {}
        }
    }

    /// Regression gate: the full real tool surface, after sanitization, must
    /// carry ZERO non-standard `format` annotations. This keeps any current or
    /// future numeric field (`u32`/`u64`/`usize`/`i32`/`f32` …) from
    /// reintroducing the `unknown format "uint32" ignored` warning flood that
    /// strict clients (Ajv) reject outright.
    #[test]
    fn real_tool_schemas_have_no_nonstandard_formats_after_sanitize() {
        let tools = sanitize_tools(super::super::SynapseService::tool_router().list_all());
        let mut offenders = Vec::new();
        for tool in &tools {
            let input = Value::Object((*tool.input_schema).clone());
            nonstandard_format_paths(
                &input,
                &format!("{}.inputSchema", tool.name),
                &mut offenders,
            );
            if let Some(output) = &tool.output_schema {
                let output = Value::Object((**output).clone());
                nonstandard_format_paths(
                    &output,
                    &format!("{}.outputSchema", tool.name),
                    &mut offenders,
                );
            }
        }
        assert!(
            offenders.is_empty(),
            "sanitized tool schemas still carry non-standard JSON Schema formats \
             (strict MCP clients reject / warn on these): {offenders:#?}"
        );
    }

    /// The raw (un-sanitized) surface IS expected to contain non-standard formats
    /// (schemars emits `uint32`/`uint`/`uint64`/… for Rust integer fields). This
    /// proves the gate above is load-bearing and not passing vacuously, and it
    /// enumerates exactly which formats the sanitizer removes.
    #[test]
    fn raw_tool_schemas_do_contain_nonstandard_formats() {
        let tools = super::super::SynapseService::tool_router().list_all();
        let mut offenders = Vec::new();
        for tool in &tools {
            let input = Value::Object((*tool.input_schema).clone());
            nonstandard_format_paths(&input, &format!("{}.in", tool.name), &mut offenders);
            if let Some(output) = &tool.output_schema {
                let output = Value::Object((**output).clone());
                nonstandard_format_paths(&output, &format!("{}.out", tool.name), &mut offenders);
            }
        }
        let distinct: std::collections::BTreeSet<&str> =
            offenders.iter().map(|(_, f)| f.as_str()).collect();
        eprintln!("raw non-standard formats stripped by sanitizer: {distinct:?}");
        assert!(
            !offenders.is_empty(),
            "expected schemars to emit at least one non-standard integer format \
             (uint32/uint/…) somewhere in the tool surface"
        );
    }

    #[test]
    fn strip_removes_nonstandard_int_format_but_keeps_constraints_and_standard_formats() {
        // Non-standard numeric format is removed; type/minimum survive.
        let mut schema = json!({
            "type": "object",
            "properties": {
                "duration_ms": { "type": "integer", "format": "uint32", "minimum": 0 },
                "ratio": { "type": "number", "format": "double" },
                "id": { "type": "string", "format": "uuid" },
                "when": { "type": "string", "format": "date-time" }
            }
        });
        rewrite_value(&mut schema);

        let props = &schema["properties"];
        // uint32 stripped, but type:integer + minimum:0 preserved.
        assert!(props["duration_ms"].get("format").is_none());
        assert_eq!(props["duration_ms"]["type"], "integer");
        assert_eq!(props["duration_ms"]["minimum"], 0);
        // double (OpenAPI, non-standard) stripped; type:number preserved.
        assert!(props["ratio"].get("format").is_none());
        assert_eq!(props["ratio"]["type"], "number");
        // Standard string formats preserved.
        assert_eq!(props["id"]["format"], "uuid");
        assert_eq!(props["when"]["format"], "date-time");
    }

    #[test]
    fn normalize_metadata_text_rewrites_known_unicode_punctuation() {
        let text = "a — b – c … x → y ← z ⇒ ok ∈ set ∉ set Σ total ±10 ×6 ≤7 ≥5 “q” ‘s’";
        let normalized = normalize_metadata_text(text);
        assert!(normalized.is_ascii(), "{normalized}");
        assert_eq!(
            normalized,
            "a - b - c ... x -> y <- z => ok in set not in set sum total +/-10 x6 <=7 >=5 \"q\" 's'"
        );
    }

    #[test]
    fn rewrite_converts_property_booleans_and_preserves_additional_properties() {
        let mut schema = json!({
            "type": "object",
            "properties": {
                "payload": true,
                "nested": {
                    "type": "object",
                    "properties": { "inner": true }
                },
                "array_payload": {
                    "type": "array",
                    "items": true
                }
            },
            "$defs": {
                "AnyJson": true
            },
            "oneOf": [ true, { "type": "string" } ],
            "not": false,
            "additionalProperties": false
        });
        rewrite_value(&mut schema);

        // properties.payload boolean -> permissive object schema
        assert!(schema["properties"]["payload"].is_object());
        assert!(schema["properties"]["payload"]["type"].is_array());
        // deeply nested properties boolean rewritten too
        assert!(schema["properties"]["nested"]["properties"]["inner"].is_object());
        // array item schemas are schema-valued and must be rewritten too
        assert!(schema["properties"]["array_payload"]["items"].is_object());
        // definition-map members are schema-valued and must be rewritten
        assert!(schema["$defs"]["AnyJson"].is_object());
        // oneOf boolean element rewritten
        assert!(schema["oneOf"][0].is_object());
        // single schema-valued keywords rewrite false to a never-matching schema
        assert!(schema["not"].is_object());
        // additionalProperties boolean preserved (meaningful, accepted by clients)
        assert_eq!(schema["additionalProperties"], Value::Bool(false));
    }
}
