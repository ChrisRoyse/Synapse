const URL_REDACTION_MARKER: &str = "redacted";

use serde_json::Value;

pub(crate) fn redact_url_for_public_readback(url: &str) -> String {
    if url.is_empty() {
        return String::new();
    }

    if let Some(rest) = strip_scheme_prefix(url, "view-source") {
        return format!("view-source:{}", redact_url_for_public_readback(rest));
    }

    for scheme in ["data", "javascript", "mailto", "blob", "filesystem"] {
        if strip_scheme_prefix(url, scheme).is_some() {
            return format!("{scheme}:{URL_REDACTION_MARKER}");
        }
    }

    if strip_scheme_prefix(url, "file").is_some() {
        return format!("file:{URL_REDACTION_MARKER}");
    }

    if let Some(rest) = strip_scheme_prefix(url, "about") {
        return redact_about_url(rest);
    }

    match reqwest::Url::parse(url) {
        Ok(parsed) => redact_parsed_url(parsed),
        Err(_error) => redact_query_and_fragment_without_parse(url),
    }
}

// Keep optional URL fields on the same public-readback policy as required fields.
#[allow(clippy::single_option_map)]
pub(crate) fn redact_url_opt_for_public_readback(url: Option<String>) -> Option<String> {
    url.map(|url| redact_url_for_public_readback(&url))
}

pub(crate) fn redact_url_fields_for_public_readback(value: &mut Value) -> usize {
    match value {
        Value::Array(items) => items
            .iter_mut()
            .map(redact_url_fields_for_public_readback)
            .sum(),
        Value::Object(fields) => {
            let mut redacted = 0;
            let has_content_bearing_url = fields.iter().any(|(key, child)| {
                is_public_url_field(key) && value_has_content_bearing_url(child)
            });
            for (key, child) in fields {
                if is_public_title_field(key) {
                    if let Some(title) = child.as_str() {
                        let public =
                            redact_title_value_for_public_readback(title, has_content_bearing_url);
                        if public != title {
                            *child = Value::String(public);
                            redacted += 1;
                        }
                        continue;
                    }
                }
                if is_public_url_field(key) {
                    if let Some(url) = child.as_str() {
                        let public = redact_url_for_public_readback(url);
                        if public != url {
                            *child = Value::String(public);
                            redacted += 1;
                        }
                        continue;
                    }
                    if let Value::Array(items) = child {
                        for item in items {
                            if let Some(url) = item.as_str() {
                                let public = redact_url_for_public_readback(url);
                                if public != url {
                                    *item = Value::String(public);
                                    redacted += 1;
                                }
                            } else {
                                redacted += redact_url_fields_for_public_readback(item);
                            }
                        }
                        continue;
                    }
                }
                redacted += redact_url_fields_for_public_readback(child);
            }
            redacted
        }
        _ => 0,
    }
}

fn value_has_content_bearing_url(value: &Value) -> bool {
    match value {
        Value::String(url) => url_has_content_bearing_scheme(url),
        Value::Array(items) => items.iter().any(value_has_content_bearing_url),
        _ => false,
    }
}

pub(crate) fn redact_title_for_public_url_readback(url: &str, title: String) -> String {
    if url_has_content_bearing_scheme(url) {
        return URL_REDACTION_MARKER.to_owned();
    }
    redact_url_like_title_for_public_readback(&title).unwrap_or(title)
}

pub(crate) fn url_has_content_bearing_scheme(url: &str) -> bool {
    if let Some(rest) = strip_scheme_prefix(url, "view-source") {
        return url_has_content_bearing_scheme(rest);
    }

    ["data", "javascript", "mailto", "file", "blob", "filesystem"]
        .iter()
        .any(|scheme| strip_scheme_prefix(url, scheme).is_some())
}

fn strip_scheme_prefix<'a>(url: &'a str, scheme: &str) -> Option<&'a str> {
    let (candidate, rest) = url.split_once(':')?;
    candidate.eq_ignore_ascii_case(scheme).then_some(rest)
}

fn redact_parsed_url(mut parsed: reqwest::Url) -> String {
    let _ = parsed.set_username("");
    let _ = parsed.set_password(None);
    if parsed.path() != "/" && !parsed.path().is_empty() {
        parsed.set_path(&format!("/{URL_REDACTION_MARKER}"));
    }
    if parsed.query().is_some() {
        parsed.set_query(Some(URL_REDACTION_MARKER));
    }
    if parsed.fragment().is_some() {
        parsed.set_fragment(Some(URL_REDACTION_MARKER));
    }
    parsed.to_string()
}

fn redact_about_url(rest: &str) -> String {
    if rest.eq_ignore_ascii_case("blank") {
        return "about:blank".to_owned();
    }
    if rest
        .get(..6)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("blank?"))
    {
        return format!("about:blank?{URL_REDACTION_MARKER}");
    }
    if rest
        .get(..6)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("blank#"))
    {
        return format!("about:blank#{URL_REDACTION_MARKER}");
    }
    format!("about:{URL_REDACTION_MARKER}")
}

fn redact_query_and_fragment_without_parse(url: &str) -> String {
    let query_index = url.find('?');
    let fragment_index = url.find('#');
    match (query_index, fragment_index) {
        (None, None) => redact_unparseable_path(url),
        (Some(query), None) => {
            format!(
                "{}?{URL_REDACTION_MARKER}",
                redact_unparseable_path(&url[..query])
            )
        }
        (None, Some(fragment)) => {
            format!(
                "{}#{URL_REDACTION_MARKER}",
                redact_unparseable_path(&url[..fragment])
            )
        }
        (Some(query), Some(fragment)) if query < fragment => {
            format!(
                "{}?{URL_REDACTION_MARKER}#{URL_REDACTION_MARKER}",
                redact_unparseable_path(&url[..query])
            )
        }
        (Some(_query), Some(fragment)) => {
            format!(
                "{}#{URL_REDACTION_MARKER}",
                redact_unparseable_path(&url[..fragment])
            )
        }
    }
}

fn redact_unparseable_path(path: &str) -> String {
    if path.is_empty() || path == "/" {
        return path.to_owned();
    }
    if path.starts_with('/') {
        return format!("/{URL_REDACTION_MARKER}");
    }
    URL_REDACTION_MARKER.to_owned()
}

fn redact_title_value_for_public_readback(title: &str, force_redact: bool) -> String {
    if force_redact && !title.is_empty() && title != URL_REDACTION_MARKER {
        return URL_REDACTION_MARKER.to_owned();
    }
    redact_url_like_title_for_public_readback(title).unwrap_or_else(|| title.to_owned())
}

fn redact_url_like_title_for_public_readback(title: &str) -> Option<String> {
    let trimmed = title.trim();
    if trimmed.is_empty() || trimmed != title {
        return None;
    }
    if let Ok(_parsed) = reqwest::Url::parse(trimmed) {
        let public = redact_url_for_public_readback(trimmed);
        return (public != trimmed).then_some(public);
    }
    if !looks_like_bare_url_path(trimmed) {
        return None;
    }
    let parsed = reqwest::Url::parse(&format!("https://{trimmed}")).ok()?;
    let public = redact_parsed_url(parsed);
    public.strip_prefix("https://").map(ToOwned::to_owned)
}

fn looks_like_bare_url_path(title: &str) -> bool {
    if title.chars().any(char::is_whitespace) {
        return false;
    }
    let Some(separator) = title.find(['/', '?', '#']) else {
        return false;
    };
    let host = &title[..separator];
    !host.is_empty()
        && host.contains('.')
        && host
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '.'))
}

fn is_public_url_field(key: &str) -> bool {
    let lowered = key.to_ascii_lowercase();
    lowered == "url"
        || lowered == "href"
        || lowered == "src"
        || lowered == "referrer"
        || lowered.ends_with("_url")
        || lowered.ends_with("url")
        || lowered.ends_with("_href")
        || lowered.ends_with("_src")
}

fn is_public_title_field(key: &str) -> bool {
    let lowered = key.to_ascii_lowercase();
    lowered == "title" || lowered.ends_with("_title") || lowered.ends_with("title")
}

#[cfg(test)]
mod tests {
    use super::{
        redact_title_for_public_url_readback, redact_url_fields_for_public_readback,
        redact_url_for_public_readback,
    };
    use serde_json::json;

    #[test]
    fn redacts_path_query_and_fragment_values() {
        let redacted = redact_url_for_public_readback(
            "https://example.test/path?body=SYNAPSE_SECRET&token=SYNAPSE_TOKEN#frag=SYNAPSE_HASH",
        );

        assert_eq!(redacted, "https://example.test/redacted?redacted#redacted");
        assert!(!redacted.contains("path"));
        assert!(!redacted.contains("SYNAPSE_SECRET"));
        assert!(!redacted.contains("SYNAPSE_TOKEN"));
        assert!(!redacted.contains("SYNAPSE_HASH"));
    }

    #[test]
    fn redacts_path_without_query_or_fragment() {
        assert_eq!(
            redact_url_for_public_readback("https://example.test/account/123"),
            "https://example.test/redacted"
        );
    }

    #[test]
    fn preserves_origin_and_root_path() {
        assert_eq!(
            redact_url_for_public_readback("https://example.test/"),
            "https://example.test/"
        );
        assert_eq!(
            redact_url_for_public_readback("https://user:secret@example.test/"),
            "https://example.test/"
        );
    }

    #[test]
    fn redacts_opaque_content_bearing_schemes() {
        assert_eq!(
            redact_url_for_public_readback("data:text/html,<main>SYNAPSE_SECRET</main>"),
            "data:redacted"
        );
        assert_eq!(
            redact_url_for_public_readback("mailto:user@example.test?subject=SYNAPSE_SECRET"),
            "mailto:redacted"
        );
        assert_eq!(
            redact_url_for_public_readback("file:///C:/Users/hotra/private.txt"),
            "file:redacted"
        );
        assert_eq!(
            redact_url_for_public_readback("blob:https://example.test/SYNAPSE_SECRET"),
            "blob:redacted"
        );
    }

    #[test]
    fn redacts_titles_for_opaque_content_bearing_schemes() {
        let redacted = redact_title_for_public_url_readback(
            "data:text/html,<title>SYNAPSE_SECRET</title>",
            "SYNAPSE_SECRET".to_owned(),
        );

        assert_eq!(redacted, "redacted");
        assert!(!redacted.contains("SYNAPSE_SECRET"));
        assert_eq!(
            redact_title_for_public_url_readback(
                "view-source:file:///C:/Users/hotra/private.txt",
                "private.txt".to_owned()
            ),
            "redacted"
        );
        assert_eq!(
            redact_title_for_public_url_readback(
                "https://example.test/path?token=SYNAPSE_SECRET",
                "Example".to_owned()
            ),
            "Example"
        );
    }

    #[test]
    fn redacts_url_like_title_paths() {
        assert_eq!(
            redact_title_for_public_url_readback(
                "https://example.test/account/123",
                "example.test/account/SYNAPSE_SECRET".to_owned()
            ),
            "example.test/redacted"
        );
        assert_eq!(
            redact_title_for_public_url_readback(
                "https://example.test/account/123",
                "https://example.test/account/SYNAPSE_SECRET?token=SYNAPSE_TOKEN".to_owned()
            ),
            "https://example.test/redacted?redacted"
        );
        assert_eq!(
            redact_title_for_public_url_readback(
                "https://example.test/account/123",
                "Example Domain".to_owned()
            ),
            "Example Domain"
        );
    }

    #[test]
    fn redacts_unparseable_path_query_and_fragment() {
        let redacted = redact_url_for_public_readback("/relative/path?token=SYNAPSE_SECRET#frag");

        assert_eq!(redacted, "/redacted?redacted#redacted");
        assert!(!redacted.contains("SYNAPSE_SECRET"));
    }

    #[test]
    fn preserves_about_blank_without_fragment_payload() {
        assert_eq!(redact_url_for_public_readback("about:blank"), "about:blank");
        assert_eq!(
            redact_url_for_public_readback("about:blank#SYNAPSE_SECRET"),
            "about:blank#redacted"
        );
        assert_eq!(
            redact_url_for_public_readback("about:srcdoc"),
            "about:redacted"
        );
    }

    #[test]
    fn redacts_url_fields_recursively() {
        let mut value = json!({
            "url": "https://example.test/account/SYNAPSE_SECRET?token=SYNAPSE_TOKEN#frag",
            "origin": "https://example.test",
            "nested": {
                "pendingUrl": "https://other.test/private/123",
                "source_of_truth": "kept"
            },
            "items": [
                { "response_url": "https://api.example.test/v1/user/42" }
            ]
        });

        let count = redact_url_fields_for_public_readback(&mut value);

        assert_eq!(count, 3);
        assert_eq!(
            value["url"],
            "https://example.test/redacted?redacted#redacted"
        );
        assert_eq!(value["origin"], "https://example.test");
        assert_eq!(value["nested"]["pendingUrl"], "https://other.test/redacted");
        assert_eq!(
            value["items"][0]["response_url"],
            "https://api.example.test/redacted"
        );
        assert_eq!(value["nested"]["source_of_truth"], "kept");
    }

    #[test]
    fn redacts_title_fields_next_to_content_bearing_urls() {
        let mut value = json!({
            "url": "data:text/html,<title>SYNAPSE_SECRET</title>",
            "title": "SYNAPSE_SECRET",
            "before_url": "file:///C:/Users/hotra/private.txt",
            "before_title": "private.txt",
            "ordinary": {
                "url": "https://example.test/account/123",
                "title": "Example Title"
            }
        });

        let count = redact_url_fields_for_public_readback(&mut value);

        assert_eq!(count, 5);
        assert_eq!(value["url"], "data:redacted");
        assert_eq!(value["title"], "redacted");
        assert_eq!(value["before_url"], "file:redacted");
        assert_eq!(value["before_title"], "redacted");
        assert_eq!(value["ordinary"]["url"], "https://example.test/redacted");
        assert_eq!(value["ordinary"]["title"], "Example Title");
        assert!(!value.to_string().contains("SYNAPSE_SECRET"));
        assert!(!value.to_string().contains("private.txt"));
    }

    #[test]
    fn redacts_url_like_titles_in_recursive_payloads() {
        let mut value = json!({
            "url": "https://example.test/account/123",
            "title": "example.test/account/SYNAPSE_SECRET?token=SYNAPSE_TOKEN",
            "nested": {
                "before_url": "https://other.test/root",
                "before_title": "https://other.test/private/SYNAPSE_SECRET#frag"
            },
            "ordinary": {
                "title": "Example Title"
            }
        });

        let count = redact_url_fields_for_public_readback(&mut value);

        assert_eq!(count, 4);
        assert_eq!(value["url"], "https://example.test/redacted");
        assert_eq!(value["title"], "example.test/redacted?redacted");
        assert_eq!(value["nested"]["before_url"], "https://other.test/redacted");
        assert_eq!(
            value["nested"]["before_title"],
            "https://other.test/redacted#redacted"
        );
        assert_eq!(value["ordinary"]["title"], "Example Title");
        assert!(!value.to_string().contains("SYNAPSE_SECRET"));
        assert!(!value.to_string().contains("SYNAPSE_TOKEN"));
    }
}
