use super::{BrowserStorageParams, STORAGE_TOOL};
use rmcp::{ErrorData, model::ErrorCode};
use serde_json::{Map, Value, json};
use synapse_core::error_codes;

const LOAD_STATE_SOURCE_OF_TRUTH: &str =
    "browser_storage load_state params before Chrome bridge mutation";

pub(super) fn validate_load_state_params(params: &BrowserStorageParams) -> Result<(), ErrorData> {
    let state = params.state.as_ref().ok_or_else(|| {
        params_error(
            "state",
            "browser_storage operation=load_state requires state",
            "pass a Playwright-style storageState object with optional cookies[] and origins[] arrays",
        )
    })?;
    validate_payload(state)
}

fn validate_payload(state: &Value) -> Result<(), ErrorData> {
    let state = state.as_object().ok_or_else(|| {
        invalid(
            "state",
            "state must be an object",
            "pass a Playwright-style storageState object, not an array or primitive value",
        )
    })?;
    validate_cookies(state)?;
    validate_origins(state)
}

fn validate_cookies(state: &Map<String, Value>) -> Result<(), ErrorData> {
    let Some(cookies) = state.get("cookies") else {
        return Ok(());
    };
    let cookies = cookies.as_array().ok_or_else(|| {
        invalid(
            "state.cookies",
            "cookies must be an array",
            "pass cookies as an array of cookie objects, or omit cookies",
        )
    })?;
    for (index, cookie) in cookies.iter().enumerate() {
        let path = format!("state.cookies[{index}]");
        let cookie = cookie.as_object().ok_or_else(|| {
            invalid(
                path.as_str(),
                "cookie entry must be an object",
                "pass each cookie as an object with name, value, and either domain or url",
            )
        })?;
        required_non_empty_string(cookie, "name", path.as_str())?;
        required_string(cookie, "value", path.as_str())?;
        optional_string(cookie, "domain", path.as_str())?;
        optional_string(cookie, "path", path.as_str())?;
        optional_bool(cookie, "secure", path.as_str())?;
        optional_bool(cookie, "httpOnly", path.as_str())?;
        optional_number(cookie, "expires", path.as_str())?;
        optional_same_site(cookie, path.as_str())?;
        let has_domain = optional_non_empty_string(cookie, "domain", path.as_str())?.is_some();
        let has_url = optional_cookie_url(cookie, path.as_str())?;
        if !has_domain && !has_url {
            return Err(invalid(
                path.as_str(),
                "cookie requires either non-empty domain or valid url before mutation",
                "add cookie.domain (for storageState cookies) or cookie.url (for Chrome cookies.set)",
            ));
        }
    }
    Ok(())
}

fn validate_origins(state: &Map<String, Value>) -> Result<(), ErrorData> {
    let Some(origins) = state.get("origins") else {
        return Ok(());
    };
    let origins = origins.as_array().ok_or_else(|| {
        invalid(
            "state.origins",
            "origins must be an array",
            "pass origins as an array of origin storage objects, or omit origins",
        )
    })?;
    for (index, origin) in origins.iter().enumerate() {
        let path = format!("state.origins[{index}]");
        let origin = origin.as_object().ok_or_else(|| {
            invalid(
                path.as_str(),
                "origin entry must be an object",
                "pass each origin as an object with origin and optional localStorage/sessionStorage arrays",
            )
        })?;
        required_non_empty_string(origin, "origin", path.as_str())?;
        validate_storage_items(origin, "localStorage", path.as_str())?;
        validate_storage_items(origin, "sessionStorage", path.as_str())?;
    }
    Ok(())
}

fn validate_storage_items(
    origin: &Map<String, Value>,
    field: &'static str,
    origin_path: &str,
) -> Result<(), ErrorData> {
    let Some(items) = origin.get(field) else {
        return Ok(());
    };
    let items = items.as_array().ok_or_else(|| {
        invalid(
            format!("{origin_path}.{field}"),
            format!("{field} must be an array"),
            format!("pass {field} as an array of {{name, value}} objects, or omit it"),
        )
    })?;
    for (index, item) in items.iter().enumerate() {
        let path = format!("{origin_path}.{field}[{index}]");
        let item = item.as_object().ok_or_else(|| {
            invalid(
                path.as_str(),
                "storage item must be an object",
                "pass each storage item as an object with string name and string value",
            )
        })?;
        required_non_empty_string(item, "name", path.as_str())?;
        required_string(item, "value", path.as_str())?;
    }
    Ok(())
}

fn required_non_empty_string<'a>(
    fields: &'a Map<String, Value>,
    field: &'static str,
    object_path: &str,
) -> Result<&'a str, ErrorData> {
    let value = required_string(fields, field, object_path)?;
    if value.trim().is_empty() {
        return Err(invalid(
            format!("{object_path}.{field}"),
            format!("{field} must be a non-empty string"),
            format!("set {object_path}.{field} to a non-empty string"),
        ));
    }
    Ok(value)
}

fn required_string<'a>(
    fields: &'a Map<String, Value>,
    field: &'static str,
    object_path: &str,
) -> Result<&'a str, ErrorData> {
    match fields.get(field) {
        Some(Value::String(value)) => Ok(value.as_str()),
        Some(_) => Err(invalid(
            format!("{object_path}.{field}"),
            format!("{field} must be a string"),
            format!("set {object_path}.{field} to a string value"),
        )),
        None => Err(invalid(
            format!("{object_path}.{field}"),
            format!("{field} is required"),
            format!("include {object_path}.{field} as a string"),
        )),
    }
}

fn optional_non_empty_string<'a>(
    fields: &'a Map<String, Value>,
    field: &'static str,
    object_path: &str,
) -> Result<Option<&'a str>, ErrorData> {
    Ok(optional_string(fields, field, object_path)?
        .map(str::trim)
        .filter(|value| !value.is_empty()))
}

fn optional_string<'a>(
    fields: &'a Map<String, Value>,
    field: &'static str,
    object_path: &str,
) -> Result<Option<&'a str>, ErrorData> {
    match fields.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.as_str())),
        Some(_) => Err(invalid(
            format!("{object_path}.{field}"),
            format!("{field} must be a string when present"),
            format!("set {object_path}.{field} to a string or omit it"),
        )),
    }
}

fn optional_bool(
    fields: &Map<String, Value>,
    field: &'static str,
    object_path: &str,
) -> Result<(), ErrorData> {
    match fields.get(field) {
        None | Some(Value::Null) | Some(Value::Bool(_)) => Ok(()),
        Some(_) => Err(invalid(
            format!("{object_path}.{field}"),
            format!("{field} must be a boolean when present"),
            format!("set {object_path}.{field} to true/false or omit it"),
        )),
    }
}

fn optional_number(
    fields: &Map<String, Value>,
    field: &'static str,
    object_path: &str,
) -> Result<(), ErrorData> {
    match fields.get(field) {
        None | Some(Value::Null) | Some(Value::Number(_)) => Ok(()),
        Some(_) => Err(invalid(
            format!("{object_path}.{field}"),
            format!("{field} must be a number when present"),
            format!("set {object_path}.{field} to a numeric Unix timestamp or omit it"),
        )),
    }
}

fn optional_same_site(cookie: &Map<String, Value>, object_path: &str) -> Result<(), ErrorData> {
    let Some(same_site) = optional_string(cookie, "sameSite", object_path)? else {
        return Ok(());
    };
    let normalized = same_site.trim().to_ascii_lowercase();
    if normalized.is_empty()
        || matches!(
            normalized.as_str(),
            "lax" | "strict" | "unspecified" | "none" | "no_restriction" | "no-restriction"
        )
    {
        return Ok(());
    }
    Err(invalid(
        format!("{object_path}.sameSite"),
        format!("unsupported cookie sameSite value {same_site:?}"),
        "use lax, strict, unspecified, none, no_restriction, no-restriction, or omit sameSite",
    ))
}

fn optional_cookie_url(cookie: &Map<String, Value>, object_path: &str) -> Result<bool, ErrorData> {
    let Some(url) = optional_non_empty_string(cookie, "url", object_path)? else {
        return Ok(false);
    };
    let parsed = reqwest::Url::parse(url).map_err(|error| {
        invalid(
            format!("{object_path}.url"),
            format!("cookie url is not parseable: {error}"),
            "set cookie.url to an absolute http(s) URL or use cookie.domain",
        )
    })?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(invalid(
            format!("{object_path}.url"),
            format!("cookie url scheme {:?} is not supported", parsed.scheme()),
            "set cookie.url to an absolute http(s) URL or use cookie.domain",
        ));
    }
    Ok(true)
}

fn invalid(
    state_path: impl Into<String>,
    detail: impl Into<String>,
    remediation: impl Into<String>,
) -> ErrorData {
    let state_path = state_path.into();
    params_error(
        state_path.clone(),
        format!(
            "browser_storage operation=load_state invalid state at {state_path}: {}",
            detail.into()
        ),
        remediation,
    )
}

fn params_error(
    state_path: impl Into<String>,
    message: impl Into<String>,
    remediation: impl Into<String>,
) -> ErrorData {
    ErrorData::new(
        ErrorCode(-32099),
        message.into(),
        Some(json!({
            "code": error_codes::TOOL_PARAMS_INVALID,
            "tool": STORAGE_TOOL,
            "operation": "load_state",
            "state_path": state_path.into(),
            "source_of_truth": LOAD_STATE_SOURCE_OF_TRUTH,
            "partial_apply_policy": "reject_before_mutation",
            "remediation": remediation.into(),
        })),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::browser_storage::{BrowserStorageOperation, BrowserStorageParams};
    use serde_json::json;

    #[test]
    fn rejects_cookie_without_domain_or_url_before_bridge() {
        let params = BrowserStorageParams {
            operation: BrowserStorageOperation::LoadState,
            state: Some(json!({
                "cookies": [{
                    "name": "issue1439_invalid_cookie",
                    "value": "known-invalid-value",
                    "path": "/",
                    "httpOnly": true,
                    "secure": true,
                    "sameSite": "lax"
                }],
                "origins": []
            })),
            ..BrowserStorageParams::default()
        };

        let error = validate_load_state_params(&params).expect_err("invalid cookie must reject");
        assert_eq!(
            error_data_str(&error, "code"),
            Some(error_codes::TOOL_PARAMS_INVALID)
        );
        assert_eq!(error_data_str(&error, "tool"), Some(STORAGE_TOOL));
        assert_eq!(error_data_str(&error, "operation"), Some("load_state"));
        assert_eq!(
            error_data_str(&error, "state_path"),
            Some("state.cookies[0]")
        );
        assert_eq!(
            error_data_str(&error, "partial_apply_policy"),
            Some("reject_before_mutation")
        );
        assert!(
            error
                .message
                .contains("cookie requires either non-empty domain or valid url"),
            "unexpected error message: {}",
            error.message
        );
    }

    #[test]
    fn rejects_mixed_valid_and_invalid_cookies_before_bridge() {
        let params = BrowserStorageParams {
            operation: BrowserStorageOperation::LoadState,
            state: Some(json!({
                "cookies": [
                    {
                        "name": "issue1439_valid_cookie",
                        "value": "valid-value",
                        "domain": "example.test",
                        "path": "/",
                        "httpOnly": true,
                        "secure": true,
                        "sameSite": "strict"
                    },
                    {
                        "name": "issue1439_invalid_cookie",
                        "value": "invalid-value",
                        "path": "/"
                    }
                ],
                "origins": []
            })),
            ..BrowserStorageParams::default()
        };

        let error = validate_load_state_params(&params).expect_err("mixed state must reject");
        assert_eq!(
            error_data_str(&error, "code"),
            Some(error_codes::TOOL_PARAMS_INVALID)
        );
        assert_eq!(
            error_data_str(&error, "state_path"),
            Some("state.cookies[1]")
        );
    }

    #[test]
    fn rejects_structurally_invalid_storage_entries() {
        let params = BrowserStorageParams {
            operation: BrowserStorageOperation::LoadState,
            state: Some(json!({
                "cookies": [],
                "origins": [{
                    "origin": "https://example.test",
                    "localStorage": {
                        "name": "not-an-array",
                        "value": "bad"
                    }
                }]
            })),
            ..BrowserStorageParams::default()
        };

        let error = validate_load_state_params(&params).expect_err("bad storage shape must reject");
        assert_eq!(
            error_data_str(&error, "code"),
            Some(error_codes::TOOL_PARAMS_INVALID)
        );
        assert_eq!(
            error_data_str(&error, "state_path"),
            Some("state.origins[0].localStorage")
        );
    }

    #[test]
    fn accepts_valid_playwright_storage_state_shape() {
        let params = BrowserStorageParams {
            operation: BrowserStorageOperation::LoadState,
            state: Some(json!({
                "cookies": [
                    {
                        "name": "issue1439_valid_cookie",
                        "value": "valid-value",
                        "domain": "example.test",
                        "path": "/",
                        "expires": -1,
                        "httpOnly": true,
                        "secure": true,
                        "sameSite": "none"
                    },
                    {
                        "name": "issue1439_url_cookie",
                        "value": "",
                        "url": "https://example.test/",
                        "sameSite": "no_restriction"
                    }
                ],
                "origins": [{
                    "origin": "https://example.test",
                    "localStorage": [{"name": "issue1439_local", "value": "known-local"}],
                    "sessionStorage": [{"name": "issue1439_session", "value": ""}]
                }]
            })),
            include_session_storage: true,
            ..BrowserStorageParams::default()
        };

        validate_load_state_params(&params).expect("valid storageState shape should pass");
    }

    fn error_data_str<'a>(error: &'a ErrorData, field: &str) -> Option<&'a str> {
        error
            .data
            .as_ref()
            .and_then(|data| data.get(field))
            .and_then(Value::as_str)
    }
}
