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
