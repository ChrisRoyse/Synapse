//! Owned-PTY capture loop (#902).
//!
//! Launches a command attached to an owned pseudoconsole (ConPTY on Windows,
//! via `portable-pty`), reads its raw VT byte stream, and drives both the
//! [`AsciicastWriter`](super::asciicast::AsciicastWriter) recording and the
//! [`ShadowScreen`](super::shadow_screen::ShadowScreen) live snapshot from it.
//!
//! The pseudoconsole is *owned* by Synapse: the child writes to a PTY we
//! created, so we see exactly what a terminal would, byte for byte — the
//! authoritative source for replay (#920) and live streaming (#914).

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use tokio::sync::broadcast;

use super::asciicast::{AsciicastHeader, AsciicastWriter};
use super::shadow_screen::ShadowScreen;

/// Outcome of a completed capture session, with the physical artifacts as the
/// source of truth.
#[derive(Clone, Debug)]
pub(crate) struct CaptureSummary {
    /// Path of the asciicast v3 recording written.
    pub asciicast_path: PathBuf,
    /// Child process exit code (0 on clean exit).
    pub exit_code: i64,
    /// Total raw bytes read from the PTY.
    pub bytes_captured: u64,
    /// Number of `o` output events recorded.
    pub output_events: u64,
    /// The shadow screen rendered to text at the moment the child exited.
    pub final_screen_text: String,
}

/// Files owned by a live capture session.
#[derive(Clone, Debug)]
pub(crate) struct CaptureArtifacts {
    pub asciicast_path: PathBuf,
    pub status_path: PathBuf,
    pub final_screen_path: PathBuf,
    pub input_audit_path: PathBuf,
}

/// Readback returned after spawning a command into an owned PTY.
#[derive(Clone, Debug)]
pub(crate) struct SpawnedCapture {
    pub process_id: u32,
    pub artifacts: CaptureArtifacts,
}

/// Ordered live event emitted by the PTY capture loop for dashboard attach.
#[derive(Clone, Debug)]
pub(crate) struct TerminalCaptureEvent {
    pub seq: u64,
    pub kind: TerminalCaptureEventKind,
}

#[derive(Clone, Debug)]
pub(crate) enum TerminalCaptureEventKind {
    Output(Vec<u8>),
    Title(String),
    Prefs(serde_json::Value),
    Exit(i64),
}

#[derive(Clone, Debug)]
pub(crate) struct TerminalCaptureSnapshot {
    pub seq: u64,
    pub process_id: u32,
    pub cols: u16,
    pub rows: u16,
    pub title: String,
    pub screen_text: String,
    pub status: TerminalCaptureStatus,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum TerminalCaptureStatus {
    Running,
    Finished { exit_code: i64 },
}

pub(crate) struct LiveTerminalSession {
    spawn_id: String,
    process_id: u32,
    artifacts: CaptureArtifacts,
    state: Mutex<LiveTerminalState>,
    master: Mutex<Option<Box<dyn MasterPty + Send>>>,
    input: Mutex<Box<dyn Write + Send>>,
    events: broadcast::Sender<TerminalCaptureEvent>,
    input_audit: Mutex<BufWriter<File>>,
}

#[derive(Debug)]
struct LiveTerminalState {
    screen: ShadowScreen,
    title: String,
    cols: u16,
    rows: u16,
    next_seq: u64,
    status: TerminalCaptureStatus,
}

static LIVE_TERMINAL_SESSIONS: OnceLock<Mutex<BTreeMap<String, Arc<LiveTerminalSession>>>> =
    OnceLock::new();

const LIVE_TERMINAL_BROADCAST_CAPACITY: usize = 16_384;

/// Specification for a capture session.
#[derive(Clone, Debug)]
pub(crate) struct CaptureSpec {
    /// Registry key for live attach. `None` keeps the session recording-only.
    pub live_key: Option<String>,
    pub program: String,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub env: BTreeMap<String, String>,
    pub cols: u16,
    pub rows: u16,
    /// Unix-seconds timestamp stamped into the asciicast header.
    pub started_unix_secs: u64,
    pub title: Option<String>,
}

impl LiveTerminalSession {
    fn new(
        spawn_id: String,
        process_id: u32,
        artifacts: CaptureArtifacts,
        master: Box<dyn MasterPty + Send>,
        input: Box<dyn Write + Send>,
        cols: u16,
        rows: u16,
        title: String,
    ) -> anyhow::Result<Arc<Self>> {
        let input_audit_file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&artifacts.input_audit_path)
            .map_err(|error| {
                anyhow::anyhow!(
                    "TERMINAL_INPUT_AUDIT_OPEN_FAILED: {}: {error}",
                    artifacts.input_audit_path.display()
                )
            })?;
        let (events, _receiver) = broadcast::channel(LIVE_TERMINAL_BROADCAST_CAPACITY);
        Ok(Arc::new(Self {
            spawn_id,
            process_id,
            artifacts,
            state: Mutex::new(LiveTerminalState {
                screen: ShadowScreen::new(cols, rows),
                title,
                cols,
                rows,
                next_seq: 0,
                status: TerminalCaptureStatus::Running,
            }),
            master: Mutex::new(Some(master)),
            input: Mutex::new(input),
            events,
            input_audit: Mutex::new(BufWriter::new(input_audit_file)),
        }))
    }

    pub(crate) fn subscribe(&self) -> broadcast::Receiver<TerminalCaptureEvent> {
        self.events.subscribe()
    }

    pub(crate) fn snapshot(&self) -> anyhow::Result<TerminalCaptureSnapshot> {
        let state = self.state.lock().map_err(|_poisoned| {
            anyhow::anyhow!("TERMINAL_CAPTURE_STATE_LOCK_POISONED: snapshot")
        })?;
        Ok(TerminalCaptureSnapshot {
            seq: state.next_seq,
            process_id: self.process_id,
            cols: state.cols,
            rows: state.rows,
            title: state.title.clone(),
            screen_text: state.screen.render_text(),
            status: state.status.clone(),
        })
    }

    fn record_output(&self, chunk: &[u8]) -> anyhow::Result<u64> {
        let seq = {
            let mut state = self.state.lock().map_err(|_poisoned| {
                anyhow::anyhow!("TERMINAL_CAPTURE_STATE_LOCK_POISONED: output")
            })?;
            state.screen.feed(chunk);
            state.next_seq = state.next_seq.saturating_add(1);
            state.next_seq
        };
        let _ = self.events.send(TerminalCaptureEvent {
            seq,
            kind: TerminalCaptureEventKind::Output(chunk.to_vec()),
        });
        Ok(seq)
    }

    fn record_title(&self, title: String) -> anyhow::Result<()> {
        let seq = {
            let mut state = self.state.lock().map_err(|_poisoned| {
                anyhow::anyhow!("TERMINAL_CAPTURE_STATE_LOCK_POISONED: title")
            })?;
            if state.title == title {
                return Ok(());
            }
            state.title = title.clone();
            state.next_seq = state.next_seq.saturating_add(1);
            state.next_seq
        };
        let _ = self.events.send(TerminalCaptureEvent {
            seq,
            kind: TerminalCaptureEventKind::Title(title),
        });
        Ok(())
    }

    pub(crate) fn write_controller_input(
        &self,
        connection_id: &str,
        bytes: &[u8],
    ) -> anyhow::Result<()> {
        if bytes.is_empty() {
            return Ok(());
        }
        {
            let state = self.state.lock().map_err(|_poisoned| {
                anyhow::anyhow!("TERMINAL_CAPTURE_STATE_LOCK_POISONED: input_status")
            })?;
            if !matches!(state.status, TerminalCaptureStatus::Running) {
                return Err(anyhow::anyhow!("TERMINAL_CAPTURE_NOT_RUNNING"));
            }
        }
        {
            let mut input = self.input.lock().map_err(|_poisoned| {
                anyhow::anyhow!("TERMINAL_CAPTURE_INPUT_LOCK_POISONED: controller_input")
            })?;
            input
                .write_all(bytes)
                .map_err(|error| anyhow::anyhow!("TERMINAL_INPUT_WRITE_FAILED: {error}"))?;
            input
                .flush()
                .map_err(|error| anyhow::anyhow!("TERMINAL_INPUT_FLUSH_FAILED: {error}"))?;
        }
        self.audit_control("input", connection_id, "controller", bytes, "ok")
    }

    pub(crate) fn audit_rejected_input(
        &self,
        connection_id: &str,
        bytes: &[u8],
        reason: &'static str,
    ) -> anyhow::Result<()> {
        self.audit_control("input", connection_id, "observer", bytes, reason)
    }

    pub(crate) fn resize(&self, connection_id: &str, cols: u16, rows: u16) -> anyhow::Result<()> {
        let cols = cols.max(1);
        let rows = rows.max(1);
        {
            let master = self.master.lock().map_err(|_poisoned| {
                anyhow::anyhow!("TERMINAL_CAPTURE_MASTER_LOCK_POISONED: resize")
            })?;
            let Some(master) = master.as_ref() else {
                return Err(anyhow::anyhow!("TERMINAL_CAPTURE_NOT_RUNNING"));
            };
            master
                .resize(PtySize {
                    cols,
                    rows,
                    pixel_width: 0,
                    pixel_height: 0,
                })
                .map_err(|error| anyhow::anyhow!("TERMINAL_RESIZE_FAILED: {error}"))?;
        }
        let seq = {
            let mut state = self.state.lock().map_err(|_poisoned| {
                anyhow::anyhow!("TERMINAL_CAPTURE_STATE_LOCK_POISONED: resize")
            })?;
            state.cols = cols;
            state.rows = rows;
            state.screen.resize(cols, rows);
            state.next_seq = state.next_seq.saturating_add(1);
            state.next_seq
        };
        self.audit_control(
            "resize",
            connection_id,
            "controller",
            format!("{cols}x{rows}").as_bytes(),
            "ok",
        )?;
        let _ = self.events.send(TerminalCaptureEvent {
            seq,
            kind: TerminalCaptureEventKind::Prefs(serde_json::json!({
                "event": "resize",
                "cols": cols,
                "rows": rows,
            })),
        });
        Ok(())
    }

    fn respond_terminal_query(
        &self,
        responder: &mut TerminalQueryResponder,
        chunk: &[u8],
    ) -> std::io::Result<()> {
        let mut input = self
            .input
            .lock()
            .map_err(|_poisoned| std::io::Error::other("terminal input lock poisoned"))?;
        responder.respond(chunk, input.as_mut())
    }

    fn close_master(&self) {
        match self.master.lock() {
            Ok(mut master) => {
                let _closed = master.take();
            }
            Err(_poisoned) => {
                tracing::warn!(
                    code = "PTY_CAPTURE_MASTER_LOCK_POISONED",
                    spawn_id = %self.spawn_id,
                    "failed to close PTY master because the lock was poisoned"
                );
            }
        }
    }

    fn mark_finished(&self, exit_code: i64) {
        let seq = match self.state.lock() {
            Ok(mut state) => {
                state.status = TerminalCaptureStatus::Finished { exit_code };
                state.next_seq = state.next_seq.saturating_add(1);
                state.next_seq
            }
            Err(_poisoned) => {
                tracing::warn!(
                    code = "PTY_CAPTURE_STATE_LOCK_POISONED",
                    spawn_id = %self.spawn_id,
                    "failed to mark live terminal session finished"
                );
                0
            }
        };
        let _ = self.events.send(TerminalCaptureEvent {
            seq,
            kind: TerminalCaptureEventKind::Exit(exit_code),
        });
    }

    fn audit_control(
        &self,
        kind: &'static str,
        connection_id: &str,
        mode: &'static str,
        payload: &[u8],
        status: &'static str,
    ) -> anyhow::Result<()> {
        let record = serde_json::json!({
            "schema_version": 1,
            "at_unix_ms": unix_time_ms_now(),
            "spawn_id": &self.spawn_id,
            "process_id": self.process_id,
            "connection_id": connection_id,
            "kind": kind,
            "mode": mode,
            "status": status,
            "bytes": payload.len(),
            "payload_b64": base64::engine::general_purpose::STANDARD.encode(payload),
            "source_of_truth": self.artifacts.input_audit_path.display().to_string(),
        });
        let mut audit = self
            .input_audit
            .lock()
            .map_err(|_poisoned| anyhow::anyhow!("TERMINAL_INPUT_AUDIT_LOCK_POISONED: {kind}"))?;
        serde_json::to_writer(&mut *audit, &record)
            .map_err(|error| anyhow::anyhow!("TERMINAL_INPUT_AUDIT_ENCODE_FAILED: {error}"))?;
        writeln!(audit)
            .map_err(|error| anyhow::anyhow!("TERMINAL_INPUT_AUDIT_WRITE_FAILED: {error}"))?;
        audit
            .flush()
            .map_err(|error| anyhow::anyhow!("TERMINAL_INPUT_AUDIT_FLUSH_FAILED: {error}"))
    }
}

pub(crate) fn terminal_capture_session(spawn_id: &str) -> Option<Arc<LiveTerminalSession>> {
    let sessions = live_terminal_sessions();
    let sessions = sessions.lock().ok()?;
    sessions.get(spawn_id).cloned()
}

fn register_live_terminal_session(session: Arc<LiveTerminalSession>) {
    match live_terminal_sessions().lock() {
        Ok(mut sessions) => {
            sessions.insert(session.spawn_id.clone(), session);
        }
        Err(_poisoned) => {
            tracing::warn!(
                code = "PTY_CAPTURE_REGISTRY_LOCK_POISONED",
                "failed to register live terminal session"
            );
        }
    }
}

fn unregister_live_terminal_session(spawn_id: &str) {
    match live_terminal_sessions().lock() {
        Ok(mut sessions) => {
            sessions.remove(spawn_id);
        }
        Err(_poisoned) => {
            tracing::warn!(
                code = "PTY_CAPTURE_REGISTRY_LOCK_POISONED",
                spawn_id,
                "failed to unregister live terminal session"
            );
        }
    }
}

fn live_terminal_sessions() -> &'static Mutex<BTreeMap<String, Arc<LiveTerminalSession>>> {
    LIVE_TERMINAL_SESSIONS.get_or_init(|| Mutex::new(BTreeMap::new()))
}

/// Starts `spec` in an owned pseudoconsole and returns immediately after the
/// child PID is known. A background waiter finalizes the physical artifacts
/// when the child exits.
pub(crate) fn spawn_capture_to_asciicast(
    spec: CaptureSpec,
    artifacts: CaptureArtifacts,
) -> anyhow::Result<SpawnedCapture> {
    let pty_system = native_pty_system();
    let size = PtySize {
        rows: spec.rows,
        cols: spec.cols,
        pixel_width: 0,
        pixel_height: 0,
    };
    let pair = pty_system
        .openpty(size)
        .map_err(|error| anyhow::anyhow!("PTY_OPEN_FAILED: {error}"))?;

    let command = command_from_spec(&spec);
    let mut child = pair
        .slave
        .spawn_command(command)
        .map_err(|error| anyhow::anyhow!("PTY_SPAWN_FAILED: {error}"))?;
    let Some(process_id) = child.process_id() else {
        kill_spawned_child(child.as_mut(), None, "process_id_unavailable");
        return Err(anyhow::anyhow!("PTY_PROCESS_ID_UNAVAILABLE"));
    };
    drop(pair.slave);
    let master = pair.master;

    let reader = match master.try_clone_reader() {
        Ok(reader) => reader,
        Err(error) => {
            kill_spawned_child(child.as_mut(), Some(process_id), "reader_clone_failed");
            return Err(anyhow::anyhow!("PTY_READER_CLONE_FAILED: {error}"));
        }
    };
    let pty_input = match master.take_writer() {
        Ok(writer) => writer,
        Err(error) => {
            kill_spawned_child(child.as_mut(), Some(process_id), "writer_take_failed");
            return Err(anyhow::anyhow!("PTY_WRITER_TAKE_FAILED: {error}"));
        }
    };
    let file = match File::create(&artifacts.asciicast_path) {
        Ok(file) => file,
        Err(error) => {
            kill_spawned_child(child.as_mut(), Some(process_id), "asciicast_create_failed");
            return Err(anyhow::anyhow!(
                "ASCIICAST_CREATE_FAILED: {}: {error}",
                artifacts.asciicast_path.display()
            ));
        }
    };
    if let Err(error) = write_capture_status_running(&artifacts, &spec, process_id) {
        kill_spawned_child(child.as_mut(), Some(process_id), "status_write_failed");
        return Err(error);
    }

    let header = AsciicastHeader {
        cols: spec.cols,
        rows: spec.rows,
        term_type: String::new(),
        timestamp: spec.started_unix_secs,
        title: spec.title.clone(),
    };
    let cols = spec.cols;
    let rows = spec.rows;
    let title = spec.title.clone().unwrap_or_default();
    let (reader_input, master_for_waiter, live_session) = match spec.live_key.clone() {
        Some(live_key) => {
            let session = match LiveTerminalSession::new(
                live_key,
                process_id,
                artifacts.clone(),
                master,
                pty_input,
                cols,
                rows,
                title,
            ) {
                Ok(session) => session,
                Err(error) => {
                    kill_spawned_child(child.as_mut(), Some(process_id), "live_session_failed");
                    return Err(error);
                }
            };
            register_live_terminal_session(Arc::clone(&session));
            (ReaderInput::Live(Arc::clone(&session)), None, Some(session))
        }
        None => (ReaderInput::Direct(pty_input), Some(master), None),
    };
    let reader_thread = spawn_reader_thread(reader, reader_input, file, header, cols, rows);
    let waiter_artifacts = artifacts.clone();
    let mut spawn_failure_killer = child.clone_killer();
    let live_key_for_spawn_failure = spec.live_key;
    std::thread::Builder::new()
        .name(format!("synapse-pty-wait-{process_id}"))
        .spawn(move || {
            let status = child.wait();
            if let Some(session) = live_session.as_ref() {
                session.close_master();
            } else {
                drop(master_for_waiter);
            }
            let outcome = reader_thread.join();
            if let Err(error) = finalize_capture_artifacts(
                &waiter_artifacts,
                live_session.as_ref(),
                process_id,
                status,
                outcome,
            ) {
                tracing::warn!(
                    code = "PTY_CAPTURE_FINALIZE_FAILED",
                    process_id,
                    error = %error,
                    "failed to finalize PTY capture artifacts"
                );
            }
            if let Some(session) = live_session.as_ref() {
                unregister_live_terminal_session(&session.spawn_id);
            }
        })
        .map_err(move |error| {
            if let Some(live_key) = live_key_for_spawn_failure.as_deref() {
                unregister_live_terminal_session(live_key);
            }
            if let Err(kill_error) = spawn_failure_killer.kill() {
                tracing::warn!(
                    code = "PTY_CAPTURE_THREAD_SPAWN_CLEANUP_FAILED",
                    process_id,
                    error = %kill_error,
                    "failed to kill PTY child after waiter thread spawn failed"
                );
            }
            anyhow::anyhow!("PTY_WAITER_THREAD_SPAWN_FAILED: {error}")
        })?;

    Ok(SpawnedCapture {
        process_id,
        artifacts,
    })
}

/// Runs `spec` to completion attached to an owned pseudoconsole, writing the
/// asciicast v3 recording to `asciicast_path` and returning a summary. Blocking:
/// the daemon runs one of these on a dedicated thread per spawned agent.
pub(crate) fn capture_to_asciicast(
    spec: &CaptureSpec,
    asciicast_path: &Path,
) -> anyhow::Result<CaptureSummary> {
    let pty_system = native_pty_system();
    let size = PtySize {
        rows: spec.rows,
        cols: spec.cols,
        pixel_width: 0,
        pixel_height: 0,
    };
    let pair = pty_system
        .openpty(size)
        .map_err(|error| anyhow::anyhow!("PTY_OPEN_FAILED: {error}"))?;

    let command = command_from_spec(spec);

    let mut child = pair
        .slave
        .spawn_command(command)
        .map_err(|error| anyhow::anyhow!("PTY_SPAWN_FAILED: {error}"))?;
    // The slave handle must be dropped after spawn so the child is the only
    // writer to the PTY.
    drop(pair.slave);
    let master = pair.master;

    let reader = master
        .try_clone_reader()
        .map_err(|error| anyhow::anyhow!("PTY_READER_CLONE_FAILED: {error}"))?;
    let pty_input = master
        .take_writer()
        .map_err(|error| anyhow::anyhow!("PTY_WRITER_TAKE_FAILED: {error}"))?;

    let file = File::create(asciicast_path).map_err(|error| {
        anyhow::anyhow!(
            "ASCIICAST_CREATE_FAILED: {}: {error}",
            asciicast_path.display()
        )
    })?;
    let header = AsciicastHeader {
        cols: spec.cols,
        rows: spec.rows,
        term_type: String::new(),
        timestamp: spec.started_unix_secs,
        title: spec.title.clone(),
    };
    let cols = spec.cols;
    let rows = spec.rows;

    // Read on a dedicated thread. On Windows ConPTY the master reader does NOT
    // return EOF until the pseudoconsole (master) is closed — which we must not
    // do until the child has exited. A single-threaded read-then-wait therefore
    // deadlocks. So: drain on a worker thread (owning the recorder + shadow
    // screen), wait for the child on this thread, THEN drop the master to force
    // the reader to EOF, then join.
    let reader_thread = spawn_reader_thread(
        reader,
        ReaderInput::Direct(pty_input),
        file,
        header,
        cols,
        rows,
    );

    capture_trace("waiting on child");
    let status = child
        .wait()
        .map_err(|error| anyhow::anyhow!("PTY_WAIT_FAILED: {error}"))?;
    let exit_code = i64::from(status.exit_code());
    capture_trace(&format!("child exited code={exit_code}; dropping master"));
    // Close the pseudoconsole so the reader thread observes EOF and finishes.
    drop(master);
    capture_trace("master dropped; joining reader thread");
    let mut outcome = reader_thread
        .join()
        .map_err(|_| anyhow::anyhow!("PTY_READER_THREAD_PANICKED"))??;
    capture_trace("reader thread joined");

    outcome
        .writer
        .record_exit(outcome.elapsed, exit_code)
        .map_err(|error| anyhow::anyhow!("ASCIICAST_EXIT_WRITE_FAILED: {error}"))?;

    Ok(CaptureSummary {
        asciicast_path: asciicast_path.to_path_buf(),
        exit_code,
        bytes_captured: outcome.bytes_captured,
        output_events: outcome.output_events,
        final_screen_text: outcome.screen.render_text(),
    })
}

fn command_from_spec(spec: &CaptureSpec) -> CommandBuilder {
    let mut command = CommandBuilder::new(&spec.program);
    for arg in &spec.args {
        command.arg(arg);
    }
    if let Some(cwd) = spec.cwd.as_ref() {
        command.cwd(cwd);
    }
    // portable-pty's CommandBuilder launches with an EMPTY environment. On
    // Windows a process started without SystemRoot/PATH fails at DLL init with
    // STATUS_DLL_INIT_FAILED (0xC0000142) before main runs.
    for (key, value) in &spec.env {
        command.env(key, value);
    }
    command
}

fn kill_spawned_child(
    child: &mut dyn portable_pty::Child,
    process_id: Option<u32>,
    reason: &'static str,
) {
    if let Err(error) = child.kill() {
        tracing::warn!(
            code = "PTY_CAPTURE_STARTUP_CLEANUP_FAILED",
            ?process_id,
            reason,
            error = %error,
            "failed to kill PTY child after capture startup failed"
        );
    }
}

fn spawn_reader_thread(
    reader: Box<dyn Read + Send>,
    input: ReaderInput,
    file: File,
    header: AsciicastHeader,
    cols: u16,
    rows: u16,
) -> std::thread::JoinHandle<anyhow::Result<ReaderOutcome>> {
    std::thread::spawn(move || -> anyhow::Result<ReaderOutcome> {
        capture_trace("reader thread started");
        let mut reader = reader;
        let mut input = input;
        let mut terminal_responder = TerminalQueryResponder::default();
        let mut sideband_parser = TerminalSidebandParser::default();
        let mut writer = AsciicastWriter::start(BufWriter::new(file), &header)
            .map_err(|error| anyhow::anyhow!("ASCIICAST_HEADER_WRITE_FAILED: {error}"))?;
        let mut screen = ShadowScreen::new(cols, rows);
        let start = Instant::now();
        let mut buffer = [0u8; 8192];
        let mut bytes_captured = 0u64;
        let mut output_events = 0u64;
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => {
                    capture_trace("reader EOF (read returned 0)");
                    break;
                }
                Ok(n) => {
                    let chunk = &buffer[..n];
                    writer
                        .record_output(start.elapsed(), chunk)
                        .map_err(|error| anyhow::anyhow!("ASCIICAST_WRITE_FAILED: {error}"))?;
                    screen.feed(chunk);
                    match &mut input {
                        ReaderInput::Direct(_pty_input) => {}
                        ReaderInput::Live(session) => {
                            session.record_output(chunk)?;
                            for event in sideband_parser.feed(chunk) {
                                match event {
                                    TerminalSidebandEvent::Title(title) => {
                                        session.record_title(title)?;
                                    }
                                    TerminalSidebandEvent::Bell => {
                                        // BEL is already preserved in the raw output frame.
                                    }
                                }
                            }
                        }
                    }
                    if let Err(error) = input.respond_terminal_query(&mut terminal_responder, chunk)
                    {
                        tracing::warn!(
                            code = "PTY_TERMINAL_QUERY_RESPONSE_FAILED",
                            error = %error,
                            "failed to answer PTY terminal query"
                        );
                    }
                    bytes_captured += n as u64;
                    output_events += 1;
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::UnexpectedEof
                    ) =>
                {
                    break;
                }
                Err(error) => return Err(anyhow::anyhow!("PTY_READ_FAILED: {error}")),
            }
        }
        Ok(ReaderOutcome {
            writer,
            screen,
            bytes_captured,
            output_events,
            elapsed: start.elapsed(),
        })
    })
}

enum ReaderInput {
    Direct(Box<dyn Write + Send>),
    Live(Arc<LiveTerminalSession>),
}

impl ReaderInput {
    fn respond_terminal_query(
        &mut self,
        responder: &mut TerminalQueryResponder,
        chunk: &[u8],
    ) -> std::io::Result<()> {
        match self {
            Self::Direct(pty_input) => responder.respond(chunk, pty_input.as_mut()),
            Self::Live(session) => session.respond_terminal_query(responder, chunk),
        }
    }
}

#[derive(Default)]
struct TerminalSidebandParser {
    state: TerminalSidebandState,
    osc: Vec<u8>,
}

#[derive(Clone, Copy, Default)]
enum TerminalSidebandState {
    #[default]
    Ground,
    Escape,
    Osc,
    OscEscape,
}

enum TerminalSidebandEvent {
    Title(String),
    Bell,
}

impl TerminalSidebandParser {
    fn feed(&mut self, bytes: &[u8]) -> Vec<TerminalSidebandEvent> {
        let mut events = Vec::new();
        for &byte in bytes {
            match self.state {
                TerminalSidebandState::Ground => match byte {
                    0x1B => self.state = TerminalSidebandState::Escape,
                    0x07 => events.push(TerminalSidebandEvent::Bell),
                    _ => {}
                },
                TerminalSidebandState::Escape => {
                    if byte == b']' {
                        self.osc.clear();
                        self.state = TerminalSidebandState::Osc;
                    } else {
                        self.state = TerminalSidebandState::Ground;
                    }
                }
                TerminalSidebandState::Osc => match byte {
                    0x07 => {
                        if let Some(title) = osc_title(&self.osc) {
                            events.push(TerminalSidebandEvent::Title(title));
                        }
                        self.osc.clear();
                        self.state = TerminalSidebandState::Ground;
                    }
                    0x1B => self.state = TerminalSidebandState::OscEscape,
                    _ => self.osc.push(byte),
                },
                TerminalSidebandState::OscEscape => {
                    if byte == b'\\' {
                        if let Some(title) = osc_title(&self.osc) {
                            events.push(TerminalSidebandEvent::Title(title));
                        }
                        self.osc.clear();
                        self.state = TerminalSidebandState::Ground;
                    } else {
                        self.osc.push(0x1B);
                        self.osc.push(byte);
                        self.state = TerminalSidebandState::Osc;
                    }
                }
            }
        }
        events
    }
}

fn osc_title(bytes: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(bytes);
    let (kind, title) = text.split_once(';')?;
    if kind == "0" || kind == "2" {
        Some(title.to_owned())
    } else {
        None
    }
}

#[derive(Default)]
struct TerminalQueryResponder {
    pending: Vec<u8>,
}

impl TerminalQueryResponder {
    fn respond(&mut self, chunk: &[u8], pty_input: &mut dyn Write) -> std::io::Result<()> {
        self.pending.extend_from_slice(chunk);
        let mut processed_until = 0;
        while let Some((offset, sequence_len, response)) =
            next_supported_terminal_query(&self.pending[processed_until..])
        {
            let sequence_start = processed_until + offset;
            pty_input.write_all(response)?;
            pty_input.flush()?;
            processed_until = sequence_start + sequence_len;
        }

        let retain_len = supported_terminal_query_max_len().saturating_sub(1);
        let retain_from = self
            .pending
            .len()
            .saturating_sub(retain_len)
            .max(processed_until);
        if retain_from > 0 {
            self.pending.drain(..retain_from);
        }
        Ok(())
    }
}

fn next_supported_terminal_query(haystack: &[u8]) -> Option<(usize, usize, &'static [u8])> {
    const CPR_RESPONSE: &[u8] = b"\x1b[1;1R";
    const QUERIES: [&[u8]; 2] = [b"\x1b[6n", b"\x1b[?6n"];
    QUERIES
        .iter()
        .filter_map(|query| find_subslice(haystack, query).map(|offset| (offset, query.len())))
        .min_by_key(|(offset, _len)| *offset)
        .map(|(offset, len)| (offset, len, CPR_RESPONSE))
}

fn supported_terminal_query_max_len() -> usize {
    b"\x1b[?6n".len()
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn finalize_capture_artifacts(
    artifacts: &CaptureArtifacts,
    live_session: Option<&Arc<LiveTerminalSession>>,
    process_id: u32,
    status: std::io::Result<portable_pty::ExitStatus>,
    outcome: std::thread::Result<anyhow::Result<ReaderOutcome>>,
) -> anyhow::Result<()> {
    let mut outcome = outcome
        .map_err(|_| anyhow::anyhow!("PTY_READER_THREAD_PANICKED"))?
        .map_err(|error| anyhow::anyhow!("PTY_READER_THREAD_FAILED: {error}"))?;
    let exit_code = status
        .map_err(|error| anyhow::anyhow!("PTY_WAIT_FAILED: {error}"))
        .map(|status| i64::from(status.exit_code()))?;
    outcome
        .writer
        .record_exit(outcome.elapsed, exit_code)
        .map_err(|error| anyhow::anyhow!("ASCIICAST_EXIT_WRITE_FAILED: {error}"))?;
    let final_screen_text = outcome.screen.render_text();
    std::fs::write(&artifacts.final_screen_path, &final_screen_text).map_err(|error| {
        anyhow::anyhow!(
            "FINAL_SCREEN_WRITE_FAILED: {}: {error}",
            artifacts.final_screen_path.display()
        )
    })?;
    let status_write = write_capture_status_finished(
        artifacts,
        process_id,
        exit_code,
        outcome.bytes_captured,
        outcome.output_events,
        final_screen_text.len(),
    );
    if let Some(session) = live_session {
        session.mark_finished(exit_code);
    }
    status_write
}

fn write_capture_status_running(
    artifacts: &CaptureArtifacts,
    spec: &CaptureSpec,
    process_id: u32,
) -> anyhow::Result<()> {
    write_capture_status_json(
        artifacts,
        serde_json::json!({
            "schema_version": 1,
            "status": "running",
            "process_id": process_id,
            "program": spec.program,
            "args": spec.args,
            "cwd": spec.cwd.as_ref().map(|path| path.display().to_string()),
            "cols": spec.cols,
            "rows": spec.rows,
            "started_unix_secs": spec.started_unix_secs,
            "title": spec.title,
            "asciicast_path": artifacts.asciicast_path.display().to_string(),
            "final_screen_path": artifacts.final_screen_path.display().to_string(),
            "input_audit_path": artifacts.input_audit_path.display().to_string(),
        }),
    )
}

fn write_capture_status_finished(
    artifacts: &CaptureArtifacts,
    process_id: u32,
    exit_code: i64,
    bytes_captured: u64,
    output_events: u64,
    final_screen_bytes: usize,
) -> anyhow::Result<()> {
    write_capture_status_json(
        artifacts,
        serde_json::json!({
            "schema_version": 1,
            "status": "finished",
            "process_id": process_id,
            "exit_code": exit_code,
            "bytes_captured": bytes_captured,
            "output_events": output_events,
            "asciicast_path": artifacts.asciicast_path.display().to_string(),
            "final_screen_path": artifacts.final_screen_path.display().to_string(),
            "input_audit_path": artifacts.input_audit_path.display().to_string(),
            "final_screen_bytes": final_screen_bytes,
        }),
    )
}

fn write_capture_status_json(
    artifacts: &CaptureArtifacts,
    status: serde_json::Value,
) -> anyhow::Result<()> {
    let bytes = serde_json::to_vec_pretty(&status)
        .map_err(|error| anyhow::anyhow!("CAPTURE_STATUS_ENCODE_FAILED: {error}"))?;
    std::fs::write(&artifacts.status_path, bytes).map_err(|error| {
        anyhow::anyhow!(
            "CAPTURE_STATUS_WRITE_FAILED: {}: {error}",
            artifacts.status_path.display()
        )
    })
}

/// Phase tracing for the capture loop, gated to the `synapse_pty_trace` cfg-less
/// env var so it is silent in production but visible when debugging a hang.
fn capture_trace(message: &str) {
    if std::env::var_os("SYNAPSE_PTY_TRACE").is_some() {
        eprintln!("[pty-capture] {message}");
    }
}

fn unix_time_ms_now() -> u64 {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    duration.as_secs().saturating_mul(1000) + u64::from(duration.subsec_millis())
}

/// What the reader thread hands back so the caller can finalize the recording.
struct ReaderOutcome {
    writer: AsciicastWriter<BufWriter<File>>,
    screen: ShadowScreen,
    bytes_captured: u64,
    output_events: u64,
    elapsed: std::time::Duration,
}

/// A live, shareable handle to a capture session's shadow screen, so the #914
/// streaming endpoint can dump the current screen on attach while the capture
/// loop keeps feeding it. (Wired into the spawn path as #914 lands.)
pub(crate) type SharedShadowScreen = Arc<Mutex<ShadowScreen>>;
