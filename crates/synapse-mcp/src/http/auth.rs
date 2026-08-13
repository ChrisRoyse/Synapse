use std::{net::SocketAddr, sync::Arc};

use axum::{
    body::Body,
    extract::State,
    http::{HeaderMap, Request, StatusCode, Uri, header, uri::Authority},
    middleware::Next,
    response::{IntoResponse, Response},
};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::bearer_token::{TokenSource, load_token};

const BRIDGE_REGISTER_TOKEN_HEADER: &str = "x-synapse-bridge-register-token";
const BRIDGE_REGISTER_TOKEN_DOMAIN: &[u8] = b"synapse.chrome_bridge.register.v1";

#[derive(Clone, Debug)]
pub(super) struct HttpAuth {
    token_digest: [u8; 32],
    bridge_register_token_digest: [u8; 32],
    source: TokenSource,
    bind_addr: SocketAddr,
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

pub(super) enum HttpSecurityDecision {
    Authorized,
    Respond(Response),
}

impl HttpAuth {
    pub(super) fn load(bind_addr: SocketAddr) -> anyhow::Result<Self> {
        let resolution = load_token()?;
        // Name the winning source in the boot log before anything can reject a
        // request for using the other one (#2099).
        resolution.report();
        let (token, source) = (resolution.token, resolution.source);
        Ok(Self {
            token_digest: digest_token(&token),
            bridge_register_token_digest: digest_token(&derive_bridge_register_token(&token)),
            source,
            bind_addr,
        })
    }

    pub(super) const fn source_label(&self) -> &'static str {
        self.source.label()
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
    match evaluate_http_security(&auth, &request) {
        HttpSecurityDecision::Authorized => next.run(request).await,
        HttpSecurityDecision::Respond(response) => response,
    }
}

pub(super) fn evaluate_http_security(
    auth: &HttpAuth,
    request: &Request<Body>,
) -> HttpSecurityDecision {
    if auth.bind_addr.ip().is_loopback() && validate_host(request.headers()).is_ok() {
        if crate::chrome_debugger_bridge::is_direct_http_extension_bridge_cors_preflight_request(
            request.method(),
            request.headers(),
            request.uri(),
        ) {
            tracing::info!(
                code = "CHROME_BRIDGE_CORS_PREFLIGHT_ACCEPTED",
                method = %request.method(),
                path = request.uri().path(),
                origin = origin_header(request.headers()).unwrap_or("<missing>"),
                access_control_request_method = request
                    .headers()
                    .get(header::ACCESS_CONTROL_REQUEST_METHOD)
                    .and_then(|value| value.to_str().ok())
                    .map(str::trim)
                    .unwrap_or("<missing>"),
                has_access_control_request_headers = request
                    .headers()
                    .contains_key(header::ACCESS_CONTROL_REQUEST_HEADERS),
                "accepted direct Chrome bridge CORS preflight"
            );
            return HttpSecurityDecision::Respond(
                crate::chrome_debugger_bridge::direct_http_bridge_cors_preflight_response(),
            );
        }
        if crate::chrome_debugger_bridge::is_direct_http_extension_bridge_request(
            request.headers(),
            request.uri(),
        ) {
            return HttpSecurityDecision::Authorized;
        }
        if crate::chrome_debugger_bridge::is_direct_http_extension_bridge_register_or_probe_request(
            request.headers(),
            request.uri(),
        ) {
            return match auth.authorize_bridge_register(request.headers()) {
                Ok(()) => HttpSecurityDecision::Authorized,
                Err(failure) => HttpSecurityDecision::Respond(unauthorized_request(
                    failure,
                    request,
                    auth.source_label(),
                )),
            };
        }
    }
    if let Err(failure) = auth.validate_origin_and_host(request.headers()) {
        return HttpSecurityDecision::Respond(forbidden(failure));
    }
    match auth.authorize(request.headers()) {
        Ok(()) => HttpSecurityDecision::Authorized,
        Err(failure) => HttpSecurityDecision::Respond(unauthorized_request(
            failure,
            request,
            auth.source_label(),
        )),
    }
}

/// Re-export of the crate-wide bearer-token loader (#2099), kept here because
/// `http::auth` used to own the resolution and in-crate callers reach it via
/// `crate::http::load_token_value`.
pub(crate) use crate::bearer_token::load_token_value;

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

fn origin_header(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
}

/// `token_source` names where *this daemon's* token came from (#2099). Without
/// it, a client authenticating with the other source's token gets an
/// unexplained 401 and the operator has nothing to compare against.
fn unauthorized_request(
    failure: AuthFailure,
    request: &Request<Body>,
    token_source: &'static str,
) -> Response {
    tracing::warn!(
        code = synapse_core::error_codes::HTTP_TOKEN_INVALID,
        reason = ?failure,
        token_source,
        method = %request.method(),
        path = request.uri().path(),
        origin = origin_header(request.headers()).unwrap_or("<missing>"),
        has_authorization = request.headers().contains_key(header::AUTHORIZATION),
        has_bridge_register_token = request.headers().contains_key(BRIDGE_REGISTER_TOKEN_HEADER),
        has_bridge_token = request.headers().contains_key("x-synapse-bridge-token"),
        has_access_control_request_method = request
            .headers()
            .contains_key(header::ACCESS_CONTROL_REQUEST_METHOD),
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
