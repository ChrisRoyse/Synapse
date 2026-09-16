//! Windows protocol activation bridge for approval toast buttons (#867).
//!
//! Toast buttons in an unpackaged desktop app can use protocol activation. The
//! child process launched by Windows never decides approvals directly; it only
//! forwards the one-time activation token to the already-running loopback daemon
//! endpoint, where the durable RocksDB row is validated and updated.

use std::{collections::BTreeMap, net::SocketAddr, process::ExitCode, time::Duration};

use anyhow::Context;

pub(crate) const APPROVAL_PROTOCOL_SCHEME: &str = "synapse-approval";

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProtocolHandlerReadback {
    pub scheme: String,
    pub command: String,
    pub url_protocol: String,
}

#[cfg(windows)]
pub(crate) fn ensure_protocol_handler_registered() -> Result<ProtocolHandlerReadback, String> {
    windows_protocol::ensure_protocol_handler_registered()
}

#[cfg(not(windows))]
pub(crate) fn ensure_protocol_handler_registered() -> Result<ProtocolHandlerReadback, String> {
    Err("approval protocol handler registration requires Windows registry support".to_owned())
}

pub(crate) async fn run_protocol_activation(uri: &str) -> anyhow::Result<ExitCode> {
    let request = ProtocolActivationRequest::parse(uri)
        .map_err(|error| anyhow::anyhow!("invalid approval protocol activation URI: {error}"))?;
    let response = reqwest::Client::new()
        .get(request.callback_url())
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .context("send approval activation callback to local daemon")?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!(
            "approval activation callback failed with HTTP {}: {}",
            status.as_u16(),
            body
        );
    }
    Ok(ExitCode::SUCCESS)
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ProtocolActivationRequest {
    bind: String,
    approval_id: String,
    activation_id: String,
    token: String,
    decision: String,
    snooze_ms: Option<u64>,
}

impl ProtocolActivationRequest {
    fn parse(uri: &str) -> Result<Self, String> {
        let normalized = uri.trim().trim_matches('"').trim();
        let without_scheme = normalized
            .strip_prefix(&format!("{APPROVAL_PROTOCOL_SCHEME}://"))
            .ok_or_else(|| {
                format!("expected {APPROVAL_PROTOCOL_SCHEME}://decide? activation URI")
            })?;
        let (route, query) = without_scheme.split_once('?').ok_or_else(|| {
            format!("expected {APPROVAL_PROTOCOL_SCHEME}://decide? activation URI")
        })?;
        if route.trim_end_matches('/') != "decide" {
            return Err(format!(
                "expected {APPROVAL_PROTOCOL_SCHEME}://decide? activation URI"
            ));
        }
        let fields = parse_query(query)?;
        let bind = required(&fields, "bind")?;
        let addr = bind
            .parse::<SocketAddr>()
            .map_err(|error| format!("bind must be host:port: {error}"))?;
        if !addr.ip().is_loopback() {
            return Err("bind must be loopback".to_owned());
        }
        let snooze_ms = match fields.get("snooze_ms").map(String::as_str) {
            Some(value) if !value.is_empty() => Some(
                value
                    .parse::<u64>()
                    .map_err(|error| format!("snooze_ms must be an integer: {error}"))?,
            ),
            _ => None,
        };
        Ok(Self {
            bind,
            approval_id: required(&fields, "approval_id")?,
            activation_id: required(&fields, "activation_id")?,
            token: required(&fields, "token")?,
            decision: required(&fields, "decision")?,
            snooze_ms,
        })
    }

    fn callback_url(&self) -> String {
        let mut url = format!(
            "http://{}/approval/activate?bind={}&approval_id={}&activation_id={}&token={}&decision={}",
            self.bind,
            url_encode(&self.bind),
            url_encode(&self.approval_id),
            url_encode(&self.activation_id),
            url_encode(&self.token),
            url_encode(&self.decision),
        );
        if let Some(snooze_ms) = self.snooze_ms {
            url.push_str("&snooze_ms=");
            url.push_str(&snooze_ms.to_string());
        }
        url
    }
}

fn required(fields: &BTreeMap<String, String>, name: &str) -> Result<String, String> {
    fields
        .get(name)
        .filter(|value| !value.is_empty())
        .cloned()
        .ok_or_else(|| format!("missing {name}"))
}

fn parse_query(raw: &str) -> Result<BTreeMap<String, String>, String> {
    let mut fields = BTreeMap::new();
    for pair in raw.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (key, value) = pair
            .split_once('=')
            .ok_or_else(|| format!("query pair {pair:?} is missing '='"))?;
        let key = url_decode(key)?;
        let value = url_decode(value)?;
        if fields.insert(key.clone(), value).is_some() {
            return Err(format!("duplicate query field {key:?}"));
        }
    }
    Ok(fields)
}

fn url_decode(value: &str) -> Result<String, String> {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' => {
                if i + 2 >= bytes.len() {
                    return Err("truncated percent escape".to_owned());
                }
                let hi = hex_value(bytes[i + 1]).ok_or_else(|| "bad percent escape".to_owned())?;
                let lo = hex_value(bytes[i + 2]).ok_or_else(|| "bad percent escape".to_owned())?;
                out.push((hi << 4) | lo);
                i += 3;
            }
            other => {
                out.push(other);
                i += 1;
            }
        }
    }
    String::from_utf8(out).map_err(|error| format!("query value is not UTF-8: {error}"))
}

fn url_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(char::from(byte));
            }
            _ => {
                const HEX: &[u8; 16] = b"0123456789ABCDEF";
                out.push('%');
                out.push(char::from(HEX[usize::from(byte >> 4)]));
                out.push(char::from(HEX[usize::from(byte & 0x0f)]));
            }
        }
    }
    out
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(windows)]
mod windows_protocol {
    use super::{APPROVAL_PROTOCOL_SCHEME, ProtocolHandlerReadback};
    use windows::{
        Win32::{
            Foundation::ERROR_SUCCESS,
            System::Registry::{
                HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE,
                REG_SZ, REG_VALUE_TYPE, RRF_RT_REG_SZ, RegCloseKey, RegCreateKeyExW, RegGetValueW,
                RegSetValueExW,
            },
        },
        core::PCWSTR,
    };

    const SCHEME_SUBKEY: &str = "Software\\Classes\\synapse-approval";
    const COMMAND_SUBKEY: &str = "Software\\Classes\\synapse-approval\\shell\\open\\command";
    const URL_PROTOCOL_VALUE: &str = "URL Protocol";

    pub(super) fn ensure_protocol_handler_registered() -> Result<ProtocolHandlerReadback, String> {
        let exe = std::env::current_exe()
            .map_err(|error| format!("read current synapse-mcp executable path: {error}"))?;
        let exe = exe.display().to_string().replace('"', "");
        let command = format!("\"{exe}\" --mode approval-protocol --approval-uri \"%1\"");
        set_registry_string(SCHEME_SUBKEY, None, "URL:Synapse Approval")?;
        set_registry_string(SCHEME_SUBKEY, Some(URL_PROTOCOL_VALUE), "")?;
        set_registry_string(COMMAND_SUBKEY, None, &command)?;

        let scheme = read_registry_string(SCHEME_SUBKEY, None)
            .ok_or_else(|| format!("missing HKCU\\{SCHEME_SUBKEY} default value"))?;
        let url_protocol = read_registry_string(SCHEME_SUBKEY, Some(URL_PROTOCOL_VALUE))
            .ok_or_else(|| format!("missing HKCU\\{SCHEME_SUBKEY}\\{URL_PROTOCOL_VALUE}"))?;
        let command_readback = read_registry_string(COMMAND_SUBKEY, None)
            .ok_or_else(|| format!("missing HKCU\\{COMMAND_SUBKEY} default value"))?;
        if scheme != "URL:Synapse Approval" {
            return Err(format!("protocol scheme readback mismatch: {scheme:?}"));
        }
        if command_readback != command {
            return Err(format!(
                "protocol command readback mismatch: expected {command:?}, found {command_readback:?}"
            ));
        }
        Ok(ProtocolHandlerReadback {
            scheme: APPROVAL_PROTOCOL_SCHEME.to_owned(),
            command: command_readback,
            url_protocol,
        })
    }

    fn set_registry_string(
        subkey: &str,
        value_name: Option<&str>,
        value: &str,
    ) -> Result<(), String> {
        let subkey_wide = wide_null(subkey);
        let mut key = HKEY::default();
        let status = unsafe {
            RegCreateKeyExW(
                HKEY_CURRENT_USER,
                PCWSTR(subkey_wide.as_ptr()),
                None,
                PCWSTR::null(),
                REG_OPTION_NON_VOLATILE,
                KEY_SET_VALUE | KEY_QUERY_VALUE,
                None,
                &raw mut key,
                None,
            )
        };
        if status != ERROR_SUCCESS {
            return Err(format!(
                "RegCreateKeyExW(HKCU\\{subkey}) failed with status {}",
                status.0
            ));
        }
        let name_wide = value_name.map(wide_null);
        let name_pcw = name_wide
            .as_ref()
            .map_or_else(PCWSTR::null, |name| PCWSTR(name.as_ptr()));
        let value_wide = wide_null(value);
        let bytes: &[u8] = unsafe {
            std::slice::from_raw_parts(value_wide.as_ptr().cast::<u8>(), value_wide.len() * 2)
        };
        let status = unsafe { RegSetValueExW(key, name_pcw, None, REG_SZ, Some(bytes)) };
        let close_status = unsafe { RegCloseKey(key) };
        if status != ERROR_SUCCESS {
            return Err(format!(
                "RegSetValueExW(HKCU\\{subkey}) failed with status {}",
                status.0
            ));
        }
        if close_status != ERROR_SUCCESS {
            return Err(format!(
                "RegCloseKey(HKCU\\{subkey}) failed with status {}",
                close_status.0
            ));
        }
        Ok(())
    }

    fn read_registry_string(subkey: &str, value_name: Option<&str>) -> Option<String> {
        let subkey_wide = wide_null(subkey);
        let value_wide = value_name.map(wide_null);
        let value_pcw = value_wide
            .as_ref()
            .map_or_else(PCWSTR::null, |value| PCWSTR(value.as_ptr()));
        let mut value_type = REG_VALUE_TYPE::default();
        let mut byte_len = 0_u32;
        let status = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                PCWSTR(subkey_wide.as_ptr()),
                value_pcw,
                RRF_RT_REG_SZ,
                Some(&raw mut value_type),
                None,
                Some(&raw mut byte_len),
            )
        };
        if status != ERROR_SUCCESS || byte_len == 0 {
            return None;
        }
        let mut buffer = vec![0_u16; (byte_len as usize).div_ceil(2)];
        let status = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                PCWSTR(subkey_wide.as_ptr()),
                value_pcw,
                RRF_RT_REG_SZ,
                Some(&raw mut value_type),
                Some(buffer.as_mut_ptr().cast()),
                Some(&raw mut byte_len),
            )
        };
        if status != ERROR_SUCCESS {
            return None;
        }
        let units = (byte_len as usize).div_ceil(2).min(buffer.len());
        buffer.truncate(units);
        let nul = buffer
            .iter()
            .position(|unit| *unit == 0)
            .unwrap_or(buffer.len());
        Some(String::from_utf16_lossy(&buffer[..nul]))
    }

    fn wide_null(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_activation_uri_parses_and_preserves_callback_fields() {
        let parsed = ProtocolActivationRequest::parse(
            "synapse-approval://decide?bind=127.0.0.1%3A7700&approval_id=apr1-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa&activation_id=actv1-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb&token=act1-cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc&decision=snooze&snooze_ms=900000",
        )
        .expect("parse");
        assert_eq!(parsed.bind, "127.0.0.1:7700");
        assert_eq!(parsed.decision, "snooze");
        assert_eq!(parsed.snooze_ms, Some(900_000));
        let callback = parsed.callback_url();
        assert!(callback.starts_with("http://127.0.0.1:7700/approval/activate?"));
        assert!(callback.contains("decision=snooze"));
        assert!(callback.contains("token=act1-cccc"));
    }

    #[test]
    fn protocol_activation_uri_accepts_windows_shell_handoff_variants() {
        for uri in [
            "\"synapse-approval://decide?bind=127.0.0.1%3A7700&approval_id=apr1-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa&activation_id=actv1-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb&token=act1-cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc&decision=accept\"",
            "synapse-approval://decide/?bind=127.0.0.1%3A7700&approval_id=apr1-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa&activation_id=actv1-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb&token=act1-cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc&decision=accept",
        ] {
            let parsed = ProtocolActivationRequest::parse(uri).expect(uri);
            assert_eq!(parsed.bind, "127.0.0.1:7700");
            assert_eq!(parsed.decision, "accept");
        }
    }

    #[test]
    fn protocol_activation_refuses_non_loopback_bind() {
        let error = ProtocolActivationRequest::parse(
            "synapse-approval://decide?bind=192.168.1.20%3A7700&approval_id=apr1-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa&activation_id=actv1-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb&token=act1-cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc&decision=accept",
        )
        .unwrap_err();
        assert!(error.contains("loopback"));
    }
}
