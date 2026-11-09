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
