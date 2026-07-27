//! Request-scoped reranker hook for the :8089 cross-encoder surface.
//!
//! PRD 30 §1/§4 (Leapable privacy invariant): reranker candidate text is
//! request-scoped and never persisted. [`RerankCandidateText`] makes that
//! structural — zeroize-on-drop, Debug-redacted, no `Display`, no serde
//! impls — and every transient buffer holding candidate bytes (wire JSON,
//! HTTP request) is zeroized too.

use std::fmt;
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use calyx_core::Result;
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::error::{
    CALYX_SEXTANT_RERANKER_ENDPOINT, CALYX_SEXTANT_RERANKER_NO_CANDIDATES,
    CALYX_SEXTANT_RERANKER_PROTOCOL, CALYX_SEXTANT_RERANKER_TIMEOUT, sextant_error,
};

/// Candidate text scoped to a single rerank request.
///
/// Must never become serializable or displayable; the only raw-text exit is
/// [`RerankCandidateText::as_str`] on the reranker wire path.
///
/// ```compile_fail
/// fn requires_serialize<T: serde::Serialize>() {}
/// requires_serialize::<calyx_sextant::RerankCandidateText>();
/// ```
///
/// ```compile_fail
/// fn requires_deserialize<T: serde::de::DeserializeOwned>() {}
/// requires_deserialize::<calyx_sextant::RerankCandidateText>();
/// ```
///
/// ```compile_fail
/// fn requires_display<T: std::fmt::Display>() {}
/// requires_display::<calyx_sextant::RerankCandidateText>();
/// ```
#[derive(PartialEq)]
pub struct RerankCandidateText {
    inner: Zeroizing<String>,
}

impl RerankCandidateText {
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            inner: Zeroizing::new(text.into()),
        }
    }

    pub fn as_str(&self) -> &str {
        self.inner.as_str()
    }
}

impl fmt::Debug for RerankCandidateText {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RerankCandidateText")
            .field("text", &"<request-scoped redacted>")
            .field("len", &self.inner.len())
            .finish()
    }
}

#[derive(PartialEq)]
pub struct RerankRequest {
    query: Zeroizing<String>,
    candidates: Vec<RerankCandidateText>,
}

impl RerankRequest {
    pub fn new(query: impl Into<String>, candidates: Vec<String>) -> Self {
        Self::from_candidate_texts(
            query,
            candidates
                .into_iter()
                .map(RerankCandidateText::new)
                .collect(),
        )
    }

    pub fn from_candidate_texts(
        query: impl Into<String>,
        candidates: Vec<RerankCandidateText>,
    ) -> Self {
        Self {
            query: Zeroizing::new(query.into()),
            candidates,
        }
    }

    pub fn query(&self) -> &str {
        self.query.as_str()
    }

    pub fn candidates(&self) -> &[RerankCandidateText] {
        &self.candidates
    }

    pub fn candidate_count(&self) -> usize {
        self.candidates.len()
    }
}

impl fmt::Debug for RerankRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RerankRequest")
            .field("query", &"<request-scoped redacted>")
            .field("candidate_count", &self.candidates.len())
            .finish()
    }
}

/// Scores only. There is deliberately no self-reported privacy flag here:
/// request-scoping is enforced by [`RerankCandidateText`] at the type level
/// and proven by the on-disk sentinel FSV, never claimed by the server.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RerankResponse {
    pub scores: Vec<f32>,
}

#[derive(Debug, Deserialize)]
struct WireRerankResponse {
    scores: Vec<f32>,
}

#[derive(Serialize)]
struct WireRerankRequest<'a> {
    query: &'a str,
    texts: Vec<&'a str>,
}

#[derive(Debug, Deserialize)]
struct WireRank {
    index: usize,
    score: f32,
}

#[derive(Clone, Debug)]
pub struct RerankerClient {
    endpoint: String,
    timeout: Duration,
}

impl RerankerClient {
    pub fn new(endpoint: impl Into<String>, timeout: Duration) -> Self {
        Self {
            endpoint: endpoint.into(),
            timeout,
        }
    }

    pub fn rerank(&self, request: &RerankRequest) -> Result<RerankResponse> {
        if request.candidates.is_empty() {
            return Err(sextant_error(
                CALYX_SEXTANT_RERANKER_NO_CANDIDATES,
                "rerank request carries no candidate text",
            ));
        }
        if !self.endpoint.starts_with("http://") {
            return Err(sextant_error(
                CALYX_SEXTANT_RERANKER_ENDPOINT,
                "only http endpoints are supported",
            ));
        }
        let without_scheme = &self.endpoint["http://".len()..];
        let host_port = without_scheme.split('/').next().unwrap_or(without_scheme);
        let addr = host_port
            .to_socket_addrs()
            .map_err(|error| {
                sextant_error(
                    CALYX_SEXTANT_RERANKER_ENDPOINT,
                    format!("bad reranker endpoint {host_port}: {error}"),
                )
            })?
            .next()
            .ok_or_else(|| {
                sextant_error(
                    CALYX_SEXTANT_RERANKER_ENDPOINT,
                    format!("reranker endpoint {host_port} resolved to no address"),
                )
            })?;
        let mut stream = TcpStream::connect_timeout(&addr, self.timeout).map_err(|error| {
            sextant_error(
                CALYX_SEXTANT_RERANKER_TIMEOUT,
                format!("connect timeout to {addr}: {error}"),
            )
        })?;
        stream
            .set_read_timeout(Some(self.timeout))
            .map_err(|error| {
                sextant_error(
                    CALYX_SEXTANT_RERANKER_TIMEOUT,
                    format!("set reranker read timeout failed: {error}"),
                )
            })?;
        stream
            .set_write_timeout(Some(self.timeout))
            .map_err(|error| {
                sextant_error(
                    CALYX_SEXTANT_RERANKER_TIMEOUT,
                    format!("set reranker write timeout failed: {error}"),
                )
            })?;
        let texts = request
            .candidates()
            .iter()
            .map(RerankCandidateText::as_str)
            .collect();
        let wire_request = WireRerankRequest {
            query: request.query(),
            texts,
        };
        let body = Zeroizing::new(serde_json::to_string(&wire_request).map_err(|error| {
            sextant_error(
                CALYX_SEXTANT_RERANKER_PROTOCOL,
                format!("serialize rerank request failed: {error}"),
            )
        })?);
        let http = Zeroizing::new(format!(
            "POST /rerank HTTP/1.1\r\nHost: {host_port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len(),
            body = *body
        ));
        stream.write_all(http.as_bytes()).map_err(|error| {
            sextant_error(
                CALYX_SEXTANT_RERANKER_TIMEOUT,
                format!("write timeout: {error}"),
            )
        })?;
        let mut response = String::new();
        stream.read_to_string(&mut response).map_err(|error| {
            sextant_error(
                CALYX_SEXTANT_RERANKER_TIMEOUT,
                format!("read timeout: {error}"),
            )
        })?;
        ensure_success_status(&response)?;
        parse_http_rerank_response(&response, request.candidate_count())
    }
}

fn ensure_success_status(response: &str) -> Result<()> {
    if response.starts_with("HTTP/1.1 2") || response.starts_with("HTTP/1.0 2") {
        return Ok(());
    }
    let status = response.lines().next().unwrap_or("missing HTTP status");
    Err(sextant_error(
        CALYX_SEXTANT_RERANKER_PROTOCOL,
        format!("reranker returned non-2xx status: {status}"),
    ))
}

fn parse_http_rerank_response(response: &str, expected_scores: usize) -> Result<RerankResponse> {
    let body = response.split("\r\n\r\n").nth(1).ok_or_else(|| {
        sextant_error(
            CALYX_SEXTANT_RERANKER_PROTOCOL,
            "reranker response missing HTTP body",
        )
    })?;
    if body.trim_start().starts_with('[') {
        return parse_tei_rank_response(body, expected_scores);
    }
    let wire: WireRerankResponse = serde_json::from_str(body).map_err(|error| {
        sextant_error(
            CALYX_SEXTANT_RERANKER_PROTOCOL,
            format!("invalid reranker JSON: {error}"),
        )
    })?;
    if wire.scores.len() != expected_scores || wire.scores.iter().any(|score| !score.is_finite()) {
        return Err(sextant_error(
            CALYX_SEXTANT_RERANKER_PROTOCOL,
            format!(
                "reranker returned invalid score vector: {} scores for {} candidates",
                wire.scores.len(),
                expected_scores
            ),
        ));
    }
    Ok(RerankResponse {
        scores: wire.scores,
    })
}

fn parse_tei_rank_response(body: &str, expected_scores: usize) -> Result<RerankResponse> {
    let ranks: Vec<WireRank> = serde_json::from_str(body).map_err(|error| {
        sextant_error(
            CALYX_SEXTANT_RERANKER_PROTOCOL,
            format!("invalid reranker JSON: {error}"),
        )
    })?;
    if ranks.len() != expected_scores {
        return Err(sextant_error(
            CALYX_SEXTANT_RERANKER_PROTOCOL,
            format!(
                "reranker returned invalid rank vector: {} ranks for {} candidates",
                ranks.len(),
                expected_scores
            ),
        ));
    }
    let mut scores = vec![f32::NAN; expected_scores];
    for rank in ranks {
        if rank.index >= expected_scores
            || !rank.score.is_finite()
            || scores[rank.index].is_finite()
        {
            return Err(sextant_error(
                CALYX_SEXTANT_RERANKER_PROTOCOL,
                format!(
                    "reranker returned invalid rank entry at index {}",
                    rank.index
                ),
            ));
        }
        scores[rank.index] = rank.score;
    }
    if scores.iter().any(|score| !score.is_finite()) {
        return Err(sextant_error(
            CALYX_SEXTANT_RERANKER_PROTOCOL,
            "reranker returned incomplete rank vector",
        ));
    }
    Ok(RerankResponse { scores })
}
