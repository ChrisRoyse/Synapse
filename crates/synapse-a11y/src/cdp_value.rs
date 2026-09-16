use serde::Serialize;

/// Serialize a CDP enum wrapper to the protocol string produced by Serde
/// `rename` attributes.
pub fn cdp_enum_str<T: Serialize>(value: &T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_default()
}

/// Serialize a CDP numeric wrapper to an `f64` when the protocol value is
/// actually numeric. Callers that expose optional evidence should keep `None`
/// instead of silently reporting epoch/zero.
pub fn cdp_number_f64<T: Serialize>(value: &T) -> Option<f64> {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_f64())
}

/// Compatibility helper for legacy required fields that cannot yet represent
/// an absent CDP number in their public response type.
pub fn cdp_number_f64_or_zero<T: Serialize>(value: &T) -> f64 {
    cdp_number_f64(value).unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::{cdp_enum_str, cdp_number_f64, cdp_number_f64_or_zero};
    use serde::Serialize;

    #[derive(Serialize)]
    enum TestProtocolEnum {
        #[serde(rename = "console-api")]
        ConsoleApi,
    }

    #[derive(Serialize)]
    struct NotANumber {
        value: &'static str,
    }

    #[test]
    fn cdp_enum_str_uses_serde_protocol_name() {
        assert_eq!(cdp_enum_str(&TestProtocolEnum::ConsoleApi), "console-api");
    }

    #[test]
    fn cdp_number_f64_distinguishes_absent_number_from_zero() {
        assert_eq!(cdp_number_f64(&12.25_f64), Some(12.25));
        assert_eq!(cdp_number_f64(&NotANumber { value: "nope" }), None);
        assert_eq!(cdp_number_f64_or_zero(&NotANumber { value: "nope" }), 0.0);
    }
}
