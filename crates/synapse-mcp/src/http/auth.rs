use std::{net::SocketAddr, path::PathBuf, sync::Arc};

use anyhow::{Context, bail};
use axum::{
    body::Body,
    extract::State,
    http::{HeaderMap, Request, StatusCode, Uri, header, uri::Authority},
    middleware::Next,
    response::{IntoResponse, Response},
};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

const TOKEN_ENV: &str = "SYNAPSE_BEARER_TOKEN";
const APPDATA_ENV: &str = "APPDATA";
const BRIDGE_REGISTER_TOKEN_HEADER: &str = "x-synapse-bridge-register-token";
const BRIDGE_REGISTER_TOKEN_DOMAIN: &[u8] = b"synapse.chrome_bridge.register.v1";

#[derive(Clone, Debug)]
pub(super) struct HttpAuth {
    token_digest: [u8; 32],
    bridge_register_token_digest: [u8; 32],
    source: TokenSource,
    bind_addr: SocketAddr,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum TokenSource {
    File(PathBuf),
    Env,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(super) enum AuthFailure {
    Missing,
    Malformed,
    Invalid,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(super) enum OriginFailure {
    HostMissing,
    HostMalformed,
    HostRefused,
    OriginMissingNonLoopback,
    OriginMalformed,
    OriginRefused,
}

impl HttpAuth {
    pub(super) fn load(bind_addr: SocketAddr) -> anyhow::Result<Self> {
        let (token, source) = load_token()?;
        Ok(Self {
            token_digest: digest_token(&token),
            bridge_register_token_digest: digest_token(&derive_bridge_register_token(&token)),
            source,
            bind_addr,
        })
    }

    #[cfg(test)]
    pub(super) fn from_token(token: &str) -> Self {
        Self {
            token_digest: digest_token(token),
            bridge_register_token_digest: digest_token(&derive_bridge_register_token(token)),
            source: TokenSource::Env,
            bind_addr: SocketAddr::from(([127, 0, 0, 1], 7700)),
        }
    }

    pub(super) const fn source_label(&self) -> &'static str {
        match self.source {
            TokenSource::File(_) => "file",
            TokenSource::Env => "env",
        }
    }

    pub(super) fn authorize(&self, headers: &HeaderMap) -> Result<(), AuthFailure> {
        let token = bearer_token(headers)?;
        if self.token_matches(token) {
            Ok(())
        } else {
            Err(AuthFailure::Invalid)
        }
    }

    pub(super) fn authorize_bridge_register(&self, headers: &HeaderMap) -> Result<(), AuthFailure> {
        let token = bridge_register_token(headers)?;
        let candidate_digest = digest_token(token);
        if bool::from(
            self.bridge_register_token_digest
                .as_slice()
                .ct_eq(candidate_digest.as_slice()),
        ) {
            Ok(())
        } else {
            Err(AuthFailure::Invalid)
        }
    }

    pub(super) fn validate_origin_and_host(
        &self,
        headers: &HeaderMap,
    ) -> Result<(), OriginFailure> {
        validate_host(headers)?;
        validate_origin(headers, self.bind_addr)
    }

    pub(super) fn token_matches(&self, candidate: &str) -> bool {
        let candidate_digest = digest_token(candidate);
        bool::from(
            self.token_digest
                .as_slice()
                .ct_eq(candidate_digest.as_slice()),
        )
    }
}

pub(super) async fn require_http_security(
    State(auth): State<Arc<HttpAuth>>,
    request: Request<Body>,
    next: Next,
) -> Response {
    if auth.bind_addr.ip().is_loopback() && validate_host(request.headers()).is_ok() {
        if crate::chrome_debugger_bridge::is_direct_http_extension_bridge_request(
            request.headers(),
            request.uri(),
        ) {
            return next.run(request).await;
        }
        if crate::chrome_debugger_bridge::is_direct_http_extension_bridge_register_request(
            request.headers(),
            request.uri(),
        ) {
            return match auth.authorize_bridge_register(request.headers()) {
                Ok(()) => next.run(request).await,
                Err(failure) => unauthorized(failure),
            };
        }
    }
    if let Err(failure) = auth.validate_origin_and_host(request.headers()) {
        return forbidden(failure);
    }
    match auth.authorize(request.headers()) {
        Ok(()) => next.run(request).await,
        Err(failure) => unauthorized(failure),
    }
}

/// Load the daemon bearer token value (file or env), for in-process consumers
/// such as the `--mode connect` bridge that must authenticate to the daemon.
pub(crate) fn load_token_value() -> anyhow::Result<String> {
    load_token().map(|(token, _source)| token)
}

fn load_token() -> anyhow::Result<(String, TokenSource)> {
    match token_file_path() {
        Some(path) if path.is_file() => {
            let token = std::fs::read_to_string(&path)
                .with_context(|| format!("read HTTP bearer token file {}", path.display()))?;
            let token = normalize_token(&token)
                .with_context(|| format!("HTTP bearer token file is empty: {}", path.display()))?;
            Ok((token, TokenSource::File(path)))
        }
        Some(_) | None => load_env_token(),
    }
}

fn load_env_token() -> anyhow::Result<(String, TokenSource)> {
    let token = std::env::var(TOKEN_ENV)
        .with_context(|| format!("{TOKEN_ENV} is unset and token.txt is absent"))?;
    let token = normalize_token(&token).with_context(|| format!("{TOKEN_ENV} is empty"))?;
    Ok((token, TokenSource::Env))
}

fn token_file_path() -> Option<PathBuf> {
    let appdata = std::env::var_os(APPDATA_ENV)?;
    Some(PathBuf::from(appdata).join("synapse").join("token.txt"))
}

fn normalize_token(raw: &str) -> anyhow::Result<String> {
    let token = raw.trim();
    if token.is_empty() {
        bail!("empty token")
    }
    Ok(token.to_owned())
}

fn bearer_token(headers: &HeaderMap) -> Result<&str, AuthFailure> {
    let raw = headers
        .get(header::AUTHORIZATION)
        .ok_or(AuthFailure::Missing)?
        .to_str()
        .map_err(|_| AuthFailure::Malformed)?
        .trim();
    let mut parts = raw.splitn(2, char::is_whitespace);
    let scheme = parts.next().ok_or(AuthFailure::Malformed)?;
    let token = parts.next().ok_or(AuthFailure::Malformed)?.trim();
    if !scheme.eq_ignore_ascii_case("Bearer") || token.is_empty() {
        return Err(AuthFailure::Malformed);
    }
    Ok(token)
}

fn bridge_register_token(headers: &HeaderMap) -> Result<&str, AuthFailure> {
    let token = headers
        .get(BRIDGE_REGISTER_TOKEN_HEADER)
        .ok_or(AuthFailure::Missing)?
        .to_str()
        .map_err(|_| AuthFailure::Malformed)?
        .trim();
    if token.is_empty() {
        return Err(AuthFailure::Malformed);
    }
    Ok(token)
}

fn validate_host(headers: &HeaderMap) -> Result<(), OriginFailure> {
    let raw = headers
        .get(header::HOST)
        .ok_or(OriginFailure::HostMissing)?
        .to_str()
        .map_err(|_| OriginFailure::HostMalformed)?
        .trim();
    let authority = Authority::try_from(raw).map_err(|_| OriginFailure::HostMalformed)?;
    if host_allowed(authority.host()) {
        Ok(())
    } else {
        Err(OriginFailure::HostRefused)
    }
}

fn validate_origin(headers: &HeaderMap, bind_addr: SocketAddr) -> Result<(), OriginFailure> {
    let Some(raw) = headers.get(header::ORIGIN) else {
        return if bind_addr.ip().is_loopback() {
            Ok(())
        } else {
            Err(OriginFailure::OriginMissingNonLoopback)
        };
    };
    let raw = raw
        .to_str()
        .map_err(|_| OriginFailure::OriginMalformed)?
        .trim();
    let uri = raw
        .parse::<Uri>()
        .map_err(|_| OriginFailure::OriginMalformed)?;
    if uri.scheme_str() != Some("http") {
        return Err(OriginFailure::OriginRefused);
    }
    let authority = uri.authority().ok_or(OriginFailure::OriginMalformed)?;
    if host_allowed(authority.host()) {
        Ok(())
    } else {
        Err(OriginFailure::OriginRefused)
    }
}

fn host_allowed(host: &str) -> bool {
    matches!(
        host.trim_start_matches('[')
            .trim_end_matches(']')
            .to_ascii_lowercase()
            .as_str(),
        "127.0.0.1" | "localhost" | "::1"
    )
}

fn digest_token(token: &str) -> [u8; 32] {
    let digest = Sha256::digest(token.as_bytes());
    let mut output = [0_u8; 32];
    output.copy_from_slice(&digest);
    output
}

fn derive_bridge_register_token(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(BRIDGE_REGISTER_TOKEN_DOMAIN);
    hasher.update([0]);
    hasher.update(token.as_bytes());
    hex_lower(&hasher.finalize())
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(char::from(HEX[(byte >> 4) as usize]));
        output.push(char::from(HEX[(byte & 0x0f) as usize]));
    }
    output
}

fn unauthorized(failure: AuthFailure) -> Response {
    tracing::warn!(
        code = synapse_core::error_codes::HTTP_TOKEN_INVALID,
        reason = ?failure,
        "HTTP bearer token rejected"
    );
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Bearer")],
        synapse_core::error_codes::HTTP_TOKEN_INVALID,
    )
        .into_response()
}

fn forbidden(failure: OriginFailure) -> Response {
    tracing::warn!(
        code = synapse_core::error_codes::HTTP_ORIGIN_REFUSED,
        reason = ?failure,
        "HTTP origin or host rejected"
    );
    (
        StatusCode::FORBIDDEN,
        synapse_core::error_codes::HTTP_ORIGIN_REFUSED,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use axum::http::{HeaderMap, HeaderValue, header};

    use super::HttpAuth;

    #[test]
    fn bearer_compare_accepts_only_exact_token() {
        let auth = HttpAuth::from_token("synapse-secret");
        assert!(auth.token_matches("synapse-secret"));
        assert!(!auth.token_matches("synapse-secreu"));
        assert!(!auth.token_matches("synapse-secret-longer"));
        assert!(!auth.token_matches(""));
    }

    #[test]
    fn bridge_register_token_is_domain_derived_from_bearer() {
        let auth = HttpAuth::from_token("synapse-secret");
        let mut headers = HeaderMap::new();
        headers.insert(
            super::BRIDGE_REGISTER_TOKEN_HEADER,
            HeaderValue::from_str(&super::derive_bridge_register_token("synapse-secret"))
                .expect("derived token is header-safe"),
        );
        assert!(auth.authorize_bridge_register(&headers).is_ok());

        headers.insert(
            super::BRIDGE_REGISTER_TOKEN_HEADER,
            HeaderValue::from_str(&super::derive_bridge_register_token("other-secret"))
                .expect("derived token is header-safe"),
        );
        assert!(auth.authorize_bridge_register(&headers).is_err());
        headers.remove(super::BRIDGE_REGISTER_TOKEN_HEADER);
        assert!(auth.authorize_bridge_register(&headers).is_err());
    }

    #[test]
    fn bearer_compare_rejects_many_prefix_variants() {
        let correct = "a".repeat(64);
        let auth = HttpAuth::from_token(&correct);
        for index in 0..10_000 {
            let prefix_len = index % correct.len();
            let mut wrong = String::with_capacity(correct.len());
            wrong.push_str(&correct[..prefix_len]);
            wrong.push('b');
            wrong.push_str(&"c".repeat(correct.len() - prefix_len - 1));
            assert!(!auth.token_matches(&wrong), "prefix_len={prefix_len}");
        }
    }

    #[test]
    fn authorization_header_accepts_bearer_case_insensitive() {
        let auth = HttpAuth::from_token("local-token");
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("bearer local-token"),
        );
        assert!(auth.authorize(&headers).is_ok());
    }

    #[test]
    fn origin_and_host_accept_loopback_and_reject_edges() {
        let auth = HttpAuth::from_token("local-token");
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, HeaderValue::from_static("127.0.0.1:7700"));
        assert!(auth.validate_origin_and_host(&headers).is_ok());

        headers.insert(header::ORIGIN, HeaderValue::from_static("http://127.0.0.1"));
        assert!(auth.validate_origin_and_host(&headers).is_ok());

        headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("http://evil.example"),
        );
        assert!(auth.validate_origin_and_host(&headers).is_err());

        headers.insert(header::HOST, HeaderValue::from_static("evil.example"));
        assert!(auth.validate_origin_and_host(&headers).is_err());
    }

    #[test]
    fn missing_origin_is_rejected_for_non_loopback_bind() {
        let mut auth = HttpAuth::from_token("local-token");
        auth.bind_addr = SocketAddr::from(([0, 0, 0, 0], 7700));
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, HeaderValue::from_static("127.0.0.1:7700"));
        assert!(auth.validate_origin_and_host(&headers).is_err());
    }

    #[test]
    fn direct_chrome_bridge_origin_does_not_bypass_bridge_tokens() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("chrome-extension://leoocgnkjnplbfdbklajepahofecgfbk"),
        );

        let register_uri = "/chrome-debugger/native/register"
            .parse()
            .expect("static uri parses");
        assert!(
            crate::chrome_debugger_bridge::is_direct_http_extension_bridge_register_request(
                &headers,
                &register_uri,
            )
        );
        assert!(
            !crate::chrome_debugger_bridge::is_direct_http_extension_bridge_request(
                &headers,
                &register_uri,
            )
        );
        assert!(
            !crate::chrome_debugger_bridge::is_direct_http_extension_bridge_request(
                &headers,
                &"/mcp".parse().expect("static uri parses"),
            )
        );

        headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("chrome-extension://aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
        );
        assert!(
            !crate::chrome_debugger_bridge::is_direct_http_extension_bridge_register_request(
                &headers,
                &"/chrome-debugger/native/register"
                    .parse()
                    .expect("static uri parses"),
            )
        );
    }
}
