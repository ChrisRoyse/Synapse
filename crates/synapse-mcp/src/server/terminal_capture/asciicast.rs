//! asciicast v3 recorder (#902, #920).
//!
//! Writes the [asciicast v3](https://docs.asciinema.org/manual/asciicast/v3/)
//! newline-delimited-JSON format: line 1 is the header object, every following
//! line is a `[interval, code, data]` event where `interval` is the time delta
//! (seconds) from the previous event.
//!
//! Two correctness details the format spec calls out, both handled here:
//!
//! 1. **Interval drift.** Rounding each delta independently to millisecond
//!    precision accumulates error over a long recording. We instead diffuse the
//!    rounding error (Bresenham-style): every interval is computed so the
//!    running sum of *written* intervals tracks the real elapsed time, never
//!    drifting more than half a millisecond from the truth.
//!
//! 2. **UTF-8 boundaries.** Raw PTY reads can split a multi-byte UTF-8 sequence
//!    across two chunks. asciicast `data` is a JSON string (must be valid
//!    UTF-8), so we hold back any incomplete trailing bytes and prepend them to
//!    the next chunk — never emitting mojibake and never dropping bytes.

use std::io::{self, Write};
use std::time::Duration;

use serde::Serialize;
use serde_json::Value;

/// asciicast v3 event-type codes.
pub(crate) const CODE_OUTPUT: &str = "o";
pub(crate) const CODE_INPUT: &str = "i";
pub(crate) const CODE_MARKER: &str = "m";
pub(crate) const CODE_RESIZE: &str = "r";
pub(crate) const CODE_EXIT: &str = "x";

/// Recording header metadata (asciicast v3 `term` block + timestamp).
#[derive(Clone, Debug, Serialize)]
pub(crate) struct AsciicastHeader {
    pub cols: u16,
    pub rows: u16,
    #[serde(default)]
    pub term_type: String,
    /// Unix seconds the recording started. Passed in (the workflow clock is not
    /// available inside deterministic contexts), never read from the wall clock
    /// here, so tests are reproducible.
    pub timestamp: u64,
    pub title: Option<String>,
}

/// Streaming asciicast v3 writer. Construct with [`AsciicastWriter::start`],
/// feed PTY output via [`AsciicastWriter::record_output`], and finish with
/// [`AsciicastWriter::record_exit`].
pub(crate) struct AsciicastWriter<W: Write> {
    writer: W,
    /// Sum of intervals already written (seconds). Used to diffuse rounding.
    written_secs: f64,
    /// Trailing bytes from the previous output chunk that did not form a
    /// complete UTF-8 sequence yet.
    pending_utf8: Vec<u8>,
    /// Elapsed time (since recording start) carried for the pending bytes, so a
    /// completed split sequence is timestamped at the chunk that began it.
    pending_elapsed: Option<Duration>,
    finished: bool,
}

impl<W: Write> AsciicastWriter<W> {
    /// Writes the header line and returns a ready writer.
    pub(crate) fn start(mut writer: W, header: &AsciicastHeader) -> io::Result<Self> {
        let term_type = if header.term_type.is_empty() {
            "xterm-256color"
        } else {
            header.term_type.as_str()
        };
        let mut header_obj = serde_json::json!({
            "version": 3,
            "term": {
                "cols": header.cols,
                "rows": header.rows,
                "type": term_type,
            },
            "timestamp": header.timestamp,
        });
        if let Some(title) = header.title.as_ref().filter(|title| !title.is_empty()) {
            header_obj["title"] = Value::String(title.clone());
        }
        writeln!(writer, "{}", serde_json::to_string(&header_obj)?)?;
        Ok(Self {
            writer,
            written_secs: 0.0,
            pending_utf8: Vec::new(),
            pending_elapsed: None,
            finished: false,
        })
    }

    /// Records an output chunk at `elapsed` since recording start. Bytes that do
    /// not yet complete a UTF-8 sequence are buffered for the next call.
    pub(crate) fn record_output(&mut self, elapsed: Duration, bytes: &[u8]) -> io::Result<()> {
        if bytes.is_empty() {
            return Ok(());
        }
        // The buffered bytes started earlier; keep their timestamp so the
        // completed sequence is attributed to when it actually began.
        let event_elapsed = self.pending_elapsed.take().unwrap_or(elapsed);
        let mut buffer = std::mem::take(&mut self.pending_utf8);
        buffer.extend_from_slice(bytes);

        let valid_up_to = match std::str::from_utf8(&buffer) {
            Ok(_) => buffer.len(),
            Err(error) => error.valid_up_to(),
        };
        if valid_up_to < buffer.len() {
            // Hold back the incomplete trailing sequence.
            self.pending_utf8 = buffer.split_off(valid_up_to);
            self.pending_elapsed = Some(event_elapsed);
        }
        if buffer.is_empty() {
            return Ok(());
        }
        // SAFETY of correctness: buffer[..valid_up_to] is valid UTF-8 by the
        // check above; `from_utf8` cannot fail here.
        let text = String::from_utf8(buffer)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        self.write_event(event_elapsed, CODE_OUTPUT, &text)
    }

    /// Records a terminal resize at `elapsed`.
    pub(crate) fn record_resize(
        &mut self,
        elapsed: Duration,
        cols: u16,
        rows: u16,
    ) -> io::Result<()> {
        self.write_event(elapsed, CODE_RESIZE, &format!("{cols}x{rows}"))
    }

    /// Records a marker event at `elapsed`.
    pub(crate) fn record_marker(&mut self, elapsed: Duration, label: &str) -> io::Result<()> {
        self.write_event(elapsed, CODE_MARKER, label)
    }

    /// Records the session exit status and finalizes the recording. Any
    /// buffered incomplete UTF-8 is flushed lossily first (a truncated trailing
    /// sequence at EOF is genuinely incomplete data, surfaced rather than lost).
    pub(crate) fn record_exit(&mut self, elapsed: Duration, exit_code: i64) -> io::Result<()> {
        self.flush_pending_lossy(elapsed)?;
        self.write_event(elapsed, CODE_EXIT, &exit_code.to_string())?;
        self.finished = true;
        self.writer.flush()
    }

    /// Flushes any buffered incomplete trailing UTF-8 bytes as a lossy output
    /// event. Called at exit; mid-stream, incomplete bytes stay buffered.
    fn flush_pending_lossy(&mut self, elapsed: Duration) -> io::Result<()> {
        if self.pending_utf8.is_empty() {
            return Ok(());
        }
        let event_elapsed = self.pending_elapsed.take().unwrap_or(elapsed);
        let bytes = std::mem::take(&mut self.pending_utf8);
        let text = String::from_utf8_lossy(&bytes).into_owned();
        self.write_event(event_elapsed, CODE_OUTPUT, &text)
    }

    /// Writes one event line with an error-diffused interval.
    fn write_event(&mut self, elapsed: Duration, code: &str, data: &str) -> io::Result<()> {
        let interval = self.next_interval(elapsed);
        // serde_json encodes `data` as a proper JSON string (escaping control
        // characters, quotes, backslashes), which is exactly the asciicast
        // requirement for the data field.
        let line = serde_json::to_string(&(interval, code, data))?;
        writeln!(self.writer, "{line}")
    }

    /// Computes the next interval (seconds, ms-rounded) using error diffusion so
    /// the running written total tracks the real elapsed time without drift.
    fn next_interval(&mut self, elapsed: Duration) -> f64 {
        let actual = elapsed.as_secs_f64();
        // Clamp to monotonic: a non-increasing elapsed yields a zero interval.
        let raw = (actual - self.written_secs).max(0.0);
        let rounded = (raw * 1000.0).round() / 1000.0;
        self.written_secs += rounded;
        rounded
    }
}
