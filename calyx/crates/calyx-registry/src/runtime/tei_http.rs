use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::str;
use std::time::Duration;

use calyx_core::{CalyxError, Input, Lens, LensId, Modality, Result, SlotShape, SlotVector};
use serde_json::{Value, json};

use crate::frozen::FrozenLensContract;
use crate::lens::ensure_input_modality;

/// Calyx-owned FP16 multilingual E5 TEI endpoint on the local GPU host.
pub const DEFAULT_TEI_ENDPOINT: &str = "http://127.0.0.1:18190/embed";
pub const LEGACY_TEI_8088_ENDPOINT: &str = "http://127.0.0.1:8088/embed";
pub const DEFAULT_TEI_MAX_BATCH: usize = 64;

/// Blocking HTTP TEI lens runtime.
#[derive(Clone, Debug)]
pub struct TeiHttpLens {
    id: LensId,
    endpoint: String,
    modality: Modality,
    dim: u32,
    timeout: Duration,
    max_batch: usize,
}

impl TeiHttpLens {
    /// Builds a TEI HTTP lens.
    pub fn new(
        name: impl Into<String>,
        endpoint: impl Into<String>,
        modality: Modality,
        dim: u32,
    ) -> Self {
        let name = name.into();
        let endpoint = endpoint.into();
        let id = FrozenLensContract::tei_http(&name, &endpoint, modality, dim).lens_id();
        Self {
            id,
            endpoint,
            modality,
            dim,
            timeout: Duration::from_secs(30),
            max_batch: DEFAULT_TEI_MAX_BATCH,
        }
    }

    /// Builds a text lens for manual's resident `:8088` TEI service.
    pub fn resident_8088(name: impl Into<String>, dim: u32) -> Self {
        Self::new(name, LEGACY_TEI_8088_ENDPOINT, Modality::Text, dim)
    }

    /// Builds a text lens for Calyx-owned resident `:18190` TEI service.
    pub fn resident_calyx_e5_18190(name: impl Into<String>, dim: u32) -> Self {
        Self::new(name, DEFAULT_TEI_ENDPOINT, Modality::Text, dim)
    }

    /// Overrides the socket timeout.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Overrides the maximum TEI request batch size.
    pub fn with_max_batch(mut self, max_batch: usize) -> Self {
        self.max_batch = max_batch.max(1);
        self
    }
}

impl Lens for TeiHttpLens {
    fn id(&self) -> LensId {
        self.id
    }

    fn shape(&self) -> SlotShape {
        SlotShape::Dense(self.dim)
    }

    fn modality(&self) -> Modality {
        self.modality
    }

    fn measure(&self, input: &Input) -> Result<SlotVector> {
        let mut batch = self.measure_batch(std::slice::from_ref(input))?;
        batch.pop().ok_or_else(|| {
            CalyxError::lens_dim_mismatch(format!("lens {} returned no TEI vector", self.id))
        })
    }

    fn measure_batch(&self, inputs: &[Input]) -> Result<Vec<SlotVector>> {
        if inputs.is_empty() {
            return Ok(Vec::new());
        }
        let mut texts = Vec::with_capacity(inputs.len());
        for input in inputs {
            ensure_input_modality(self, input)?;
            texts.push(text_from_input(input)?);
        }

        let mut vectors = Vec::with_capacity(inputs.len());
        for chunk in texts.chunks(self.max_batch) {
            // Complete input is part of the frozen measurement. Explicit false
            // prevents a server default or upgrade from silently substituting
            // a prefix; an oversized request must fail with the service error.
            let body = serde_json::to_vec(&json!({ "inputs": chunk, "truncate": false })).map_err(
                |err| CalyxError::lens_unreachable(format!("TEI request encode failed: {err}")),
            )?;
            let raw = post_json(&self.endpoint, &body, self.timeout)?;
            vectors.extend(parse_embedding_response(&raw)?);
        }
        if vectors.len() != inputs.len() {
            return Err(CalyxError::lens_dim_mismatch(format!(
                "TEI returned {} vectors for {} inputs",
                vectors.len(),
                inputs.len()
            )));
        }
        vectors
            .into_iter()
            .map(|data| self.slot_from_row(data))
            .collect()
    }
}

fn text_from_input(input: &Input) -> Result<&str> {
    str::from_utf8(&input.bytes)
        .map_err(|err| CalyxError::lens_dim_mismatch(format!("TEI input is not UTF-8: {err}")))
}

fn post_json(endpoint: &str, body: &[u8], timeout: Duration) -> Result<Vec<u8>> {
    let endpoint = HttpEndpoint::parse(endpoint)?;
    let address = (endpoint.host.as_str(), endpoint.port)
        .to_socket_addrs()
        .map_err(|err| CalyxError::lens_unreachable(format!("resolve TEI endpoint failed: {err}")))?
        .next()
        .ok_or_else(|| CalyxError::lens_unreachable("TEI endpoint resolved no addresses"))?;
    let mut stream = TcpStream::connect_timeout(&address, timeout).map_err(|err| {
        CalyxError::lens_unreachable(format!("connect TEI endpoint failed: {err}"))
    })?;
    stream.set_read_timeout(Some(timeout)).map_err(|err| {
        CalyxError::lens_unreachable(format!("set TEI read timeout failed: {err}"))
    })?;
    stream.set_write_timeout(Some(timeout)).map_err(|err| {
        CalyxError::lens_unreachable(format!("set TEI write timeout failed: {err}"))
    })?;

    let request = format!(
        "POST {} HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nAccept: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        endpoint.path,
        endpoint.authority(),
        body.len()
    );
    stream
        .write_all(request.as_bytes())
        .and_then(|_| stream.write_all(body))
        .map_err(|err| CalyxError::lens_unreachable(format!("write TEI request failed: {err}")))?;

    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .map_err(|err| CalyxError::lens_unreachable(format!("read TEI response failed: {err}")))?;
    parse_http_response(&response)
}

fn parse_http_response(response: &[u8]) -> Result<Vec<u8>> {
    let split = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or_else(|| CalyxError::lens_unreachable("TEI response missing header terminator"))?;
    let headers = str::from_utf8(&response[..split])
        .map_err(|err| CalyxError::lens_unreachable(format!("TEI headers invalid UTF-8: {err}")))?;
    let status = headers.lines().next().unwrap_or_default();
    if !status.contains(" 200 ") {
        let preview = String::from_utf8_lossy(&response[split + 4..]);
        return Err(CalyxError::lens_unreachable(format!(
            "TEI HTTP status {status}: {}",
            preview.chars().take(120).collect::<String>()
        )));
    }
    let body = &response[split + 4..];
    if headers
        .lines()
        .any(|line| line.eq_ignore_ascii_case("transfer-encoding: chunked"))
    {
        decode_chunked(body)
    } else {
        Ok(body.to_vec())
    }
}

fn decode_chunked(mut body: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let line_end = body
            .windows(2)
            .position(|window| window == b"\r\n")
            .ok_or_else(|| CalyxError::lens_unreachable("TEI chunk size missing CRLF"))?;
        let size_text = str::from_utf8(&body[..line_end])
            .map_err(|err| CalyxError::lens_unreachable(format!("TEI chunk size UTF-8: {err}")))?;
        let size_hex = size_text.split(';').next().unwrap_or_default();
        let size = usize::from_str_radix(size_hex.trim(), 16).map_err(|err| {
            CalyxError::lens_unreachable(format!("TEI chunk size parse failed: {err}"))
        })?;
        body = &body[line_end + 2..];
        if size == 0 {
            return Ok(out);
        }
        if body.len() < size + 2 {
            return Err(CalyxError::lens_unreachable("TEI chunk body truncated"));
        }
        out.extend_from_slice(&body[..size]);
        body = &body[size + 2..];
    }
}

fn parse_embedding_response(body: &[u8]) -> Result<Vec<Vec<f32>>> {
    let value: Value = serde_json::from_slice(body)
        .map_err(|err| CalyxError::lens_unreachable(format!("TEI JSON parse failed: {err}")))?;
    parse_vectors(&value).ok_or_else(|| {
        CalyxError::lens_dim_mismatch("TEI response did not contain embedding vectors")
    })
}

fn parse_vectors(value: &Value) -> Option<Vec<Vec<f32>>> {
    if let Some(values) = value.as_array() {
        if values.is_empty() {
            return Some(Vec::new());
        }
        if let Some(vector) = number_array(values) {
            return Some(vec![vector]);
        }
        return values.iter().map(vector_from_value).collect();
    }

    if let Some(embedding) = value.get("embedding").and_then(Value::as_array) {
        return number_array(embedding).map(|vector| vec![vector]);
    }

    if let Some(rows) = value.get("data").and_then(Value::as_array) {
        let mut vectors = Vec::with_capacity(rows.len());
        for row in rows {
            vectors.push(vector_from_value(row.get("embedding")?)?);
        }
        return Some(vectors);
    }

    None
}

fn vector_from_value(value: &Value) -> Option<Vec<f32>> {
    value.as_array().and_then(|values| number_array(values))
}

fn number_array(values: &[Value]) -> Option<Vec<f32>> {
    let mut out = Vec::with_capacity(values.len());
    for value in values {
        let number = value.as_f64()?;
        if !number.is_finite() || number < f32::MIN as f64 || number > f32::MAX as f64 {
            return None;
        }
        out.push(number as f32);
    }
    Some(out)
}

impl TeiHttpLens {
    fn slot_from_row(&self, data: Vec<f32>) -> Result<SlotVector> {
        if data.len() != self.dim as usize {
            return Err(CalyxError::lens_dim_mismatch(format!(
                "TEI dim {} != expected {}",
                data.len(),
                self.dim
            )));
        }
        if data.iter().any(|value| !value.is_finite()) {
            return Err(CalyxError::lens_numerical_invariant(
                "TEI vector contains NaN or Inf",
            ));
        }
        Ok(SlotVector::Dense {
            dim: self.dim,
            data,
        })
    }
}

struct HttpEndpoint {
    host: String,
    port: u16,
    path: String,
}

impl HttpEndpoint {
    fn parse(endpoint: &str) -> Result<Self> {
        let rest = endpoint.strip_prefix("http://").ok_or_else(|| {
            CalyxError::lens_unreachable("TEI endpoint must use http:// for PH17 runtime")
        })?;
        let (authority, path) = rest
            .split_once('/')
            .map(|(authority, path)| (authority, format!("/{path}")))
            .unwrap_or((rest, "/".to_string()));
        let (host, port) = authority
            .rsplit_once(':')
            .map(|(host, port)| {
                let parsed = port.parse::<u16>().map_err(|err| {
                    CalyxError::lens_unreachable(format!("TEI port parse failed: {err}"))
                })?;
                Ok((host.to_string(), parsed))
            })
            .unwrap_or_else(|| Ok((authority.to_string(), 80)))?;
        if host.is_empty() {
            return Err(CalyxError::lens_unreachable("TEI endpoint host is empty"));
        }
        Ok(Self { host, port, path })
    }

    fn authority(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}
