use std::{
    collections::BTreeMap,
    fmt, fs,
    path::{Path, PathBuf},
    sync::mpsc,
    time::Instant,
};

use chrono::{DateTime, Utc};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use rmcp::ErrorData;
use sha2::{Digest as _, Sha256};
use synapse_action::{ClipboardFormat, read_clipboard_text};
use synapse_core::{
    AccessibleNode, AudioContext, ClipboardSummary, DetectedEntity, FocusedElement,
    ForegroundContext, FsEvent, FsEventKind, HudReading, HudReadings, HudValue, PerceptionMode,
    Rect, SensorStatus, UiaPattern, element_id, entity_id,
};
use synapse_perception::ObservationInput;

pub const TIMELINE_CLIPBOARD_ENV: &str = "SYNAPSE_TIMELINE_CLIPBOARD";
pub const TIMELINE_FILE_ACTIVITY_ENV: &str = "SYNAPSE_TIMELINE_FILE_ACTIVITY";
const FS_WATCH_ROOT_ENV: &str = "SYNAPSE_FS_WATCH_ROOT";
const FS_WATCH_ROOTS_ENV: &str = "SYNAPSE_FS_WATCH_ROOTS";
const MAX_CLIPBOARD_SNIPPET_CHARS: usize = 256;
const MAX_FS_RECENT_EVENTS: usize = 32;
#[cfg(windows)]
const CHROMIUM_RENDERER_UIA_SUPPLEMENT_MAX_NODES: usize = 160;
#[cfg(windows)]
const A11Y_TARGET_WINDOW_MINIMIZED_UIA_UNAVAILABLE: &str =
    synapse_core::error_codes::A11Y_TARGET_WINDOW_MINIMIZED_UIA_UNAVAILABLE;
#[cfg(windows)]
const A11Y_UIA_WORKER_TIMEOUT: &str = synapse_core::error_codes::A11Y_UIA_WORKER_TIMEOUT;
#[cfg(windows)]
const A11Y_TARGET_WINDOW_NO_UIA_CONTENT: &str = "A11Y_TARGET_WINDOW_NO_UIA_CONTENT";
#[cfg(windows)]
const A11Y_TARGET_WINDOW_SNAPSHOT_FAILED: &str = "A11Y_TARGET_WINDOW_SNAPSHOT_FAILED";
#[cfg(windows)]
const MINIMIZED_WINDOW_CAPTURE_PROBE_TIMEOUT_MS: u64 = 250;

pub fn synthetic_notepad_input() -> ObservationInput {
    let at = Utc::now();
    let focused_id = element_id(0x1234, "0000002a00000001");
    let elements = vec![
        node(0, 0, "Notepad", "Window", false),
        node(1, 1, "Document", "Edit", true),
        node(2, 1, "File", "MenuItem", false),
        node(3, 1, "Edit", "MenuItem", false),
        node(4, 1, "View", "MenuItem", false),
        node(5, 1, "Status", "Text", false),
    ];
    let mut latency = BTreeMap::new();
    latency.insert("a11y".to_owned(), 1.25);
    latency.insert("capture".to_owned(), 0.50);
    ObservationInput {
        foreground: ForegroundContext {
            hwnd: 0x1234,
            pid: 44,
            process_name: "notepad.exe".to_owned(),
            process_path: "C:\\Windows\\System32\\notepad.exe".to_owned(),
            window_title: "manual.txt - Notepad".to_owned(),
            window_bounds: Rect {
                x: 10,
                y: 20,
                w: 800,
                h: 600,
            },
            monitor_index: 0,
            dpi_scale: 1.0,
            profile_id: None,
            steam_appid: None,
            is_fullscreen: false,
            is_dwm_composed: true,
        },
        is_minimized: false,
        focused: Some(FocusedElement {
            element_id: focused_id,
            name: "Document".to_owned(),
            role: "Edit".to_owned(),
            automation_id: Some("15".to_owned()),
            bbox: Rect {
                x: 12,
                y: 80,
                w: 760,
                h: 480,
            },
            enabled: true,
            patterns: vec![UiaPattern::Text, UiaPattern::Value],
            value: Some("Synthetic Synapse text".to_owned()),
            selected_text: None,
        }),
        elements,
        entities: vec![DetectedEntity {
            entity_id: entity_id(9),
            track_id: 9,
            class_label: "cursor".to_owned(),
            bbox: Rect {
                x: 40,
                y: 90,
                w: 8,
                h: 20,
            },
            confidence: 0.80,
            first_seen_at: at,
            last_seen_at: at,
            velocity_px_per_s: None,
        }],
        hud: HudReadings::default(),
        audio: AudioContext::default(),
        recent_events: Vec::new(),
        clipboard_summary: None,
        fs_recent: Vec::new(),
        sensor_latency_ms: latency,
        a11y_status: SensorStatus::Healthy,
        capture_status: SensorStatus::Healthy,
        detection_status: SensorStatus::Disabled,
        audio_status: SensorStatus::Disabled,
        mode_override: None,
        capture_config: None,
        capture_runtime: None,
        input_backends: None,
        cdp: None,
        web_path: None,
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClipboardTimelineSample {
    pub formats: Vec<String>,
    pub text_len: u32,
    pub snippet: String,
    pub text_sha256: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FsTimelineEvent {
    pub at: DateTime<Utc>,
    pub path: String,
    pub kind: FsEventKind,
    pub size_bytes: Option<u64>,
}

pub fn timeline_clipboard_enabled() -> bool {
    env_bool_default_true(TIMELINE_CLIPBOARD_ENV)
}

pub fn timeline_file_activity_enabled() -> bool {
    env_bool_default_true(TIMELINE_FILE_ACTIVITY_ENV)
}

pub fn populate_clipboard_summary(input: &mut ObservationInput) -> Option<ClipboardTimelineSample> {
    let started = Instant::now();
    let (summary, sample) = match read_clipboard_text(ClipboardFormat::Unicode) {
        Ok(text) => (
            clipboard_summary_from_text(&text),
            clipboard_timeline_sample_from_text(&text),
        ),
        Err(error) => {
            tracing::debug!(
                code = "OBSERVE_CLIPBOARD_READ_FAILED",
                error = %error,
                "clipboard summary read failed"
            );
            (
                ClipboardSummary {
                    formats: Vec::new(),
                    text_len: None,
                    text_excerpt: None,
                    redacted: true,
                },
                None,
            )
        }
    };
    input.sensor_latency_ms.insert(
        "clipboard".to_owned(),
        started.elapsed().as_secs_f32() * 1000.0,
    );
    input.clipboard_summary = Some(summary);
    sample
}

fn clipboard_summary_from_text(text: &str) -> ClipboardSummary {
    if text.is_empty() {
        return ClipboardSummary {
            formats: Vec::new(),
            text_len: None,
            text_excerpt: None,
            redacted: true,
        };
    }
    ClipboardSummary {
        formats: vec!["text/plain".to_owned(), "text/unicode".to_owned()],
        text_len: Some(len_to_u32(text.chars().count())),
        text_excerpt: Some(sha256_hex(text.as_bytes())),
        redacted: true,
    }
}

fn clipboard_timeline_sample_from_text(text: &str) -> Option<ClipboardTimelineSample> {
    if !timeline_clipboard_enabled() || text.is_empty() {
        return None;
    }
    let text_len = len_to_u32(text.chars().count());
    let snippet: String = text.chars().take(MAX_CLIPBOARD_SNIPPET_CHARS).collect();
    Some(ClipboardTimelineSample {
        formats: vec!["text/plain".to_owned(), "text/unicode".to_owned()],
        text_len,
        snippet,
        text_sha256: sha256_hex(text.as_bytes()),
    })
}

fn env_bool_default_true(name: &str) -> bool {
    let Some(value) = std::env::var_os(name) else {
        return true;
    };
    let value = value.to_string_lossy();
    match value.trim().to_ascii_lowercase().as_str() {
        "" | "1" | "true" | "yes" | "on" => true,
        "0" | "false" | "no" | "off" => false,
        other => {
            tracing::warn!(
                code = "TIMELINE_CONFIG_INVALID_BOOL",
                env = name,
                value = other,
                "timeline enrichment env flag is invalid; disabling that feed"
            );
            false
        }
    }
}

fn len_to_u32(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    format!("sha256:{}", hex_lower(&digest))
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from(HEX[usize::from(byte >> 4)]));
        out.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    out
}

pub struct FsRecentTracker {
    roots: Vec<PathBuf>,
    rx: Option<mpsc::Receiver<notify::Result<notify::Event>>>,
    _watcher: Option<RecommendedWatcher>,
    disabled_reason: Option<String>,
}

impl fmt::Debug for FsRecentTracker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FsRecentTracker")
            .field("roots", &self.roots)
            .field("enabled", &self.rx.is_some())
            .field("disabled_reason", &self.disabled_reason)
            .finish_non_exhaustive()
    }
}

impl FsRecentTracker {
    #[must_use]
    pub fn from_env() -> Self {
        if !timeline_file_activity_enabled() {
            return Self::disabled(None);
        }
        let roots = fs_watch_roots_from_env();
        if roots.is_empty() {
            return Self::disabled(None);
        }
        match Self::watch(&roots) {
            Ok(tracker) => tracker,
            Err(error) => {
                tracing::debug!(
                    code = "OBSERVE_FS_WATCH_UNAVAILABLE",
                    error = %error,
                    "filesystem summary watcher unavailable"
                );
                Self::disabled(Some(error.to_string()))
            }
        }
    }

    fn watch(raw_roots: &[PathBuf]) -> anyhow::Result<Self> {
        let mut roots = Vec::new();
        for raw_root in raw_roots {
            match fs::canonicalize(raw_root) {
                Ok(root) if root.is_dir() && !roots.contains(&root) => roots.push(root),
                Ok(root) => tracing::debug!(
                    code = "OBSERVE_FS_WATCH_ROOT_SKIPPED",
                    root = %root.display(),
                    "filesystem summary root is not a directory or is duplicate"
                ),
                Err(error) => tracing::debug!(
                    code = "OBSERVE_FS_WATCH_ROOT_UNAVAILABLE",
                    root = %raw_root.display(),
                    error = %error,
                    "filesystem summary root is unavailable"
                ),
            }
        }
        if roots.is_empty() {
            anyhow::bail!("no configured filesystem summary roots are available");
        }
        let (tx, rx) = mpsc::channel();
        let mut watcher = notify::recommended_watcher(move |event| {
            let _ = tx.send(event);
        })?;
        for root in &roots {
            watcher.watch(root, RecursiveMode::Recursive)?;
        }
        Ok(Self {
            roots,
            rx: Some(rx),
            _watcher: Some(watcher),
            disabled_reason: None,
        })
    }

    #[expect(
        clippy::missing_const_for_fn,
        reason = "constructor accepts an owned diagnostic string on the disabled path"
    )]
    fn disabled(reason: Option<String>) -> Self {
        Self {
            roots: Vec::new(),
            rx: None,
            _watcher: None,
            disabled_reason: reason,
        }
    }

    pub fn populate(&self, input: &mut ObservationInput) -> Vec<FsTimelineEvent> {
        let started = Instant::now();
        let events = self.drain_events();
        input
            .sensor_latency_ms
            .insert("fs".to_owned(), started.elapsed().as_secs_f32() * 1000.0);
        input.fs_recent = events
            .iter()
            .map(|event| event.observation.clone())
            .collect();
        events.into_iter().map(|event| event.timeline).collect()
    }

    fn drain_events(&self) -> Vec<FsObservedEvent> {
        let Some(rx) = self.rx.as_ref() else {
            return Vec::new();
        };
        let mut events = Vec::new();
        while let Ok(result) = rx.try_recv() {
            match result {
                Ok(event) => events.extend(fs_events_from_notify(&self.roots, &event)),
                Err(error) => tracing::debug!(
                    code = "OBSERVE_FS_WATCH_EVENT_FAILED",
                    error = %error,
                    "filesystem watcher event failed"
                ),
            }
        }
        let mut events = coalesce_fs_observed_events(events);
        if events.len() > MAX_FS_RECENT_EVENTS {
            events.drain(0..events.len() - MAX_FS_RECENT_EVENTS);
        }
        events
    }
}

pub fn populate_fs_recent(
    input: &mut ObservationInput,
    tracker: &FsRecentTracker,
) -> Vec<FsTimelineEvent> {
    tracker.populate(input)
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct FsObservedEvent {
    observation: FsEvent,
    timeline: FsTimelineEvent,
}

fn fs_events_from_notify(roots: &[PathBuf], event: &notify::Event) -> Vec<FsObservedEvent> {
    let Some(kind) = fs_event_kind(event.kind) else {
        return Vec::new();
    };
    let at = Utc::now();
    event
        .paths
        .iter()
        .filter_map(|path| {
            let root = event_root_for_path(roots, path)?;
            let full_path = fs_event_full_path(path);
            let full_path_text = fs_timeline_path_text(&full_path);
            let size_bytes = fs_event_size(&full_path, kind);
            Some(FsObservedEvent {
                observation: FsEvent {
                    at,
                    path: redacted_fs_path_token(root, &full_path),
                    kind,
                    size_bytes,
                },
                timeline: FsTimelineEvent {
                    at,
                    path: full_path_text,
                    kind,
                    size_bytes,
                },
            })
        })
        .collect()
}

fn coalesce_fs_observed_events(events: Vec<FsObservedEvent>) -> Vec<FsObservedEvent> {
    let mut by_path = BTreeMap::<String, FsObservedEvent>::new();
    for event in events {
        by_path
            .entry(event.timeline.path.clone())
            .and_modify(|existing| {
                existing.observation.at = event.observation.at;
                existing.timeline.at = event.timeline.at;
                let kind = coalesced_fs_kind(existing.timeline.kind, event.timeline.kind);
                existing.observation.kind = kind;
                existing.timeline.kind = kind;
                if event.timeline.size_bytes.is_some() || kind == FsEventKind::Deleted {
                    existing.observation.size_bytes = event.observation.size_bytes;
                    existing.timeline.size_bytes = event.timeline.size_bytes;
                }
            })
            .or_insert(event);
    }
    by_path.into_values().collect()
}

const fn coalesced_fs_kind(existing: FsEventKind, next: FsEventKind) -> FsEventKind {
    match (existing, next) {
        (_, FsEventKind::Deleted) | (FsEventKind::Deleted, _) => FsEventKind::Deleted,
        (FsEventKind::Created, FsEventKind::Created | FsEventKind::Modified)
        | (FsEventKind::Modified, FsEventKind::Created) => FsEventKind::Created,
        (_, FsEventKind::Renamed) | (FsEventKind::Renamed, _) => FsEventKind::Renamed,
        (_, FsEventKind::Modified) => FsEventKind::Modified,
    }
}

const fn fs_event_kind(kind: notify::EventKind) -> Option<FsEventKind> {
    match kind {
        notify::EventKind::Create(_) => Some(FsEventKind::Created),
        notify::EventKind::Modify(notify::event::ModifyKind::Name(_)) => Some(FsEventKind::Renamed),
        notify::EventKind::Modify(_) => Some(FsEventKind::Modified),
        notify::EventKind::Remove(_) => Some(FsEventKind::Deleted),
        _ => None,
    }
}

fn event_root_for_path<'a>(roots: &'a [PathBuf], path: &Path) -> Option<&'a Path> {
    roots
        .iter()
        .map(PathBuf::as_path)
        .find(|root| path.starts_with(root))
}

fn redacted_fs_path_token(root: &Path, path: &Path) -> String {
    let relative = path.strip_prefix(root).unwrap_or(path);
    let normalized = relative.to_string_lossy().replace('\\', "/");
    sha256_hex(normalized.as_bytes())
}

fn fs_event_full_path(path: &Path) -> PathBuf {
    path.canonicalize()
        .unwrap_or_else(|_error| path.to_path_buf())
}

fn fs_timeline_path_text(path: &Path) -> String {
    let text = path.to_string_lossy();
    text.strip_prefix(r"\\?\").unwrap_or(&text).to_owned()
}

fn fs_event_size(path: &Path, kind: FsEventKind) -> Option<u64> {
    if kind == FsEventKind::Deleted {
        return None;
    }
    fs::metadata(path)
        .ok()
        .filter(std::fs::Metadata::is_file)
        .map(|metadata| metadata.len())
}

fn fs_watch_roots_from_env() -> Vec<PathBuf> {
    if let Some(raw_roots) = std::env::var_os(FS_WATCH_ROOTS_ENV) {
        return split_root_list(&raw_roots.to_string_lossy());
    }
    if let Some(raw_root) = std::env::var_os(FS_WATCH_ROOT_ENV) {
        let raw = raw_root.to_string_lossy().trim().to_owned();
        if raw.is_empty() {
            return Vec::new();
        }
        return vec![PathBuf::from(raw)];
    }
    default_user_document_roots()
}

fn split_root_list(raw_roots: &str) -> Vec<PathBuf> {
    raw_roots
        .split(';')
        .map(str::trim)
        .map(|entry| entry.trim_matches('"'))
        .filter(|entry| !entry.is_empty())
        .map(PathBuf::from)
        .collect()
}

fn default_user_document_roots() -> Vec<PathBuf> {
    let Some(user_profile) = std::env::var_os("USERPROFILE") else {
        return Vec::new();
    };
    let user_profile = PathBuf::from(user_profile);
    ["Desktop", "Documents", "Downloads"]
        .into_iter()
        .map(|name| user_profile.join(name))
        .collect()
}

fn node(sequence: u32, depth: u32, name: &str, role: &str, focused: bool) -> AccessibleNode {
    let depth_i32 = i32::try_from(depth).unwrap_or(0);
    let sequence_i32 = i32::try_from(sequence).unwrap_or(0);
    AccessibleNode {
        element_id: element_id(0x1234, &format!("0000002a{sequence:08x}")),
        parent: (depth > 0).then(|| element_id(0x1234, "0000002a00000000")),
        name: name.to_owned(),
        role: role.to_owned(),
        automation_id: None,
        value: None,
        bbox: Rect {
            x: 10 + depth_i32,
            y: 20 + sequence_i32.saturating_mul(10),
            w: 100,
            h: 30,
        },
        enabled: true,
        focused,
        patterns: Vec::new(),
        children_count: 0,
        depth,
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
pub fn platform_input(_depth: u32, mode: PerceptionMode) -> Result<ObservationInput, ErrorData> {
    linux_x11::platform_input(mode)
}

#[cfg(not(any(windows, all(unix, not(target_os = "macos")))))]
pub fn platform_input(_depth: u32, _mode: PerceptionMode) -> Result<ObservationInput, ErrorData> {
    Err(crate::m1::mcp_error(
        synapse_core::error_codes::OBSERVE_NO_PERCEPTION_AVAILABLE,
        "UIA foreground window lookup requires Windows",
    ))
}

#[cfg(not(windows))]
pub fn window_input_from_hwnd(
    _hwnd: i64,
    _depth: u32,
    _mode: PerceptionMode,
) -> Result<ObservationInput, ErrorData> {
    Err(crate::m1::mcp_error(
        synapse_core::error_codes::OBSERVE_NO_PERCEPTION_AVAILABLE,
        "find window_hwnd targeting requires Windows UI Automation",
    ))
}

#[cfg(not(windows))]
pub fn element_input_from_id(
    _element_id: &synapse_core::ElementId,
    _depth: u32,
    _mode: PerceptionMode,
) -> Result<ObservationInput, ErrorData> {
    Err(crate::m1::mcp_error(
        synapse_core::error_codes::OBSERVE_NO_PERCEPTION_AVAILABLE,
        "observe subtree_root targeting requires Windows UI Automation",
    ))
}

#[cfg(all(unix, not(target_os = "macos")))]
mod linux_x11 {
    use std::{collections::BTreeMap, fs, path::PathBuf, time::Instant};

    use chrono::Utc;
    use rmcp::ErrorData;
    use synapse_core::{
        AccessibleNode, DetectedEntity, ForegroundContext, PerceptionMode, Rect, SensorStatus,
        element_id, entity_id, error_codes,
    };
    use synapse_perception::ObservationInput;
    use x11rb::{
        connection::Connection,
        protocol::xproto::{Atom, AtomEnum, ConnectionExt as _, Window},
        rust_connection::RustConnection,
    };

    use crate::m1::mcp_error;

    pub fn platform_input(mode: PerceptionMode) -> Result<ObservationInput, ErrorData> {
        let started = Instant::now();
        let (conn, screen_num) = RustConnection::connect(None)
            .map_err(|err| unavailable(format!("X11 connect failed: {err}")))?;
        let screen =
            conn.setup().roots.get(screen_num).ok_or_else(|| {
                unavailable(format!("X11 screen index {screen_num} was not found"))
            })?;
        let root_bounds = Rect {
            x: 0,
            y: 0,
            w: i32::from(screen.width_in_pixels),
            h: i32::from(screen.height_in_pixels),
        };
        let active = active_window(&conn, screen.root);
        let window = active.unwrap_or(screen.root);
        let bounds = window_bounds(&conn, window).unwrap_or(root_bounds);
        let pid = window_pid(&conn, window).unwrap_or_default();
        let title = window_title(&conn, window)
            .filter(|title| !title.is_empty())
            .unwrap_or_else(|| {
                if active.is_some() {
                    format!("X11 window 0x{window:x}")
                } else {
                    format!(
                        "X11 display {}",
                        std::env::var("DISPLAY").unwrap_or_default()
                    )
                }
            });
        let (process_name, process_path) = process_metadata(pid);
        let foreground = ForegroundContext {
            hwnd: i64::from(window),
            pid,
            process_name,
            process_path,
            window_title: title,
            window_bounds: bounds,
            monitor_index: u32::try_from(screen_num).unwrap_or(u32::MAX),
            dpi_scale: 1.0,
            profile_id: None,
            steam_appid: None,
            is_fullscreen: bounds.x <= root_bounds.x
                && bounds.y <= root_bounds.y
                && bounds.w >= root_bounds.w
                && bounds.h >= root_bounds.h,
            is_dwm_composed: false,
        };
        let mut input = ObservationInput::new(foreground.clone());
        input.elements = vec![window_node(&foreground)];
        input.focused = None;
        input.entities = cursor_entity().into_iter().collect();
        input.a11y_status = SensorStatus::DegradedSensorFailed {
            reason_code: "LINUX_X11_WINDOW_METADATA_ONLY".to_owned(),
        };
        input.capture_status = SensorStatus::Healthy;
        input.detection_status = SensorStatus::Disabled;
        input.audio_status = SensorStatus::Disabled;
        input.mode_override = Some(mode);
        input.sensor_latency_ms = BTreeMap::from([(
            "x11_window_metadata".to_owned(),
            started.elapsed().as_secs_f32() * 1000.0,
        )]);
        Ok(input)
    }

    fn active_window(conn: &RustConnection, root: Window) -> Option<Window> {
        let atom = intern_atom(conn, b"_NET_ACTIVE_WINDOW").ok()?;
        let reply = conn
            .get_property(false, root, atom, AtomEnum::WINDOW, 0, 1)
            .ok()?
            .reply()
            .ok()?;
        let window = reply.value32()?.next()?;
        if window == 0 || window_bounds(conn, window).is_none() {
            None
        } else {
            Some(window)
        }
    }

    fn window_bounds(conn: &RustConnection, window: Window) -> Option<Rect> {
        let geometry = conn.get_geometry(window).ok()?.reply().ok()?;
        Some(Rect {
            x: i32::from(geometry.x),
            y: i32::from(geometry.y),
            w: i32::from(geometry.width),
            h: i32::from(geometry.height),
        })
    }

    fn window_title(conn: &RustConnection, window: Window) -> Option<String> {
        let utf8 = intern_atom(conn, b"UTF8_STRING").ok()?;
        let net_wm_name = intern_atom(conn, b"_NET_WM_NAME").ok()?;
        read_string_property(conn, window, net_wm_name, utf8).or_else(|| {
            read_string_property(
                conn,
                window,
                AtomEnum::WM_NAME.into(),
                AtomEnum::STRING.into(),
            )
        })
    }

    fn window_pid(conn: &RustConnection, window: Window) -> Option<u32> {
        let atom = intern_atom(conn, b"_NET_WM_PID").ok()?;
        let reply = conn
            .get_property(false, window, atom, AtomEnum::CARDINAL, 0, 1)
            .ok()?
            .reply()
            .ok()?;
        reply.value32()?.next()
    }

    fn read_string_property(
        conn: &RustConnection,
        window: Window,
        property: Atom,
        property_type: Atom,
    ) -> Option<String> {
        let reply = conn
            .get_property(false, window, property, property_type, 0, 4096)
            .ok()?
            .reply()
            .ok()?;
        let bytes = trim_nul(reply.value);
        if bytes.is_empty() {
            return None;
        }
        String::from_utf8(bytes).ok()
    }

    fn intern_atom(conn: &RustConnection, name: &[u8]) -> Result<Atom, ErrorData> {
        conn.intern_atom(false, name)
            .map_err(|err| unavailable(format!("X11 intern_atom failed: {err}")))?
            .reply()
            .map(|reply| reply.atom)
            .map_err(|err| unavailable(format!("X11 intern_atom reply failed: {err}")))
    }

    fn trim_nul(mut bytes: Vec<u8>) -> Vec<u8> {
        while bytes.last() == Some(&0) {
            bytes.pop();
        }
        bytes
    }

    fn window_node(foreground: &ForegroundContext) -> AccessibleNode {
        AccessibleNode {
            element_id: element_id(foreground.hwnd, "0000000000000000"),
            parent: None,
            name: foreground.window_title.clone(),
            role: "Window".to_owned(),
            automation_id: None,
            value: None,
            bbox: foreground.window_bounds,
            enabled: true,
            focused: false,
            patterns: Vec::new(),
            children_count: 0,
            depth: 0,
        }
    }

    fn cursor_entity() -> Option<DetectedEntity> {
        let point = synapse_action::backend::software::cursor_position().ok()?;
        let at = Utc::now();
        Some(DetectedEntity {
            entity_id: entity_id(0),
            track_id: 0,
            class_label: "cursor".to_owned(),
            bbox: Rect {
                x: point.x,
                y: point.y,
                w: 1,
                h: 1,
            },
            confidence: 1.0,
            first_seen_at: at,
            last_seen_at: at,
            velocity_px_per_s: None,
        })
    }

    fn process_metadata(pid: u32) -> (String, String) {
        if pid == 0 {
            return (
                "x11".to_owned(),
                std::env::var("DISPLAY").unwrap_or_default(),
            );
        }
        let comm_path = PathBuf::from(format!("/proc/{pid}/comm"));
        let name = fs::read_to_string(comm_path)
            .ok()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| format!("pid:{pid}"));
        let process_path = fs::read_link(format!("/proc/{pid}/exe"))
            .ok()
            .map(|path| path.display().to_string())
            .unwrap_or_default();
        (name, process_path)
    }

    fn unavailable(detail: String) -> ErrorData {
        mcp_error(error_codes::OBSERVE_NO_PERCEPTION_AVAILABLE, detail)
    }
}

#[cfg(windows)]
pub fn platform_input(depth: u32, mode: PerceptionMode) -> Result<ObservationInput, ErrorData> {
    let foreground = synapse_a11y::current_foreground_context().map_err(|err| a11y_error(&err))?;
    let hwnd = foreground.hwnd;
    let tree = synapse_a11y::snapshot_window_from_hwnd(hwnd, depth).map_err(|err| {
        if err.code() == A11Y_UIA_WORKER_TIMEOUT {
            uia_worker_timeout_error(&foreground, "platform_input.snapshot_focused_window", &err)
        } else {
            a11y_error(&err)
        }
    })?;
    let mut input = input_from_tree_and_foreground(
        tree,
        foreground,
        mode,
        FocusSupplementScope::GlobalForeground,
    )?;
    input.is_minimized = synapse_a11y::is_window_minimized(hwnd).unwrap_or(false);
    Ok(input)
}

#[cfg(windows)]
pub fn window_input_from_hwnd(
    hwnd: i64,
    depth: u32,
    mode: PerceptionMode,
) -> Result<ObservationInput, ErrorData> {
    validate_live_target_hwnd(hwnd)?;
    let foreground = windows_foreground_context(hwnd)?;
    if synapse_a11y::is_window_minimized(hwnd).map_err(|err| a11y_error(&err))? {
        let mut input = match synapse_a11y::snapshot_window_from_hwnd(hwnd, depth) {
            Ok(tree) => {
                let mut input = input_from_tree_and_foreground(
                    tree,
                    foreground,
                    mode,
                    FocusSupplementScope::TargetWindow(hwnd),
                )?;
                input.is_minimized = true;
                mark_sparse_minimized_target_a11y(&mut input);
                input
            }
            Err(error) => {
                if error.code() == A11Y_UIA_WORKER_TIMEOUT {
                    return Err(uia_worker_timeout_error(
                        &foreground,
                        "window_input_from_hwnd.minimized_snapshot",
                        &error,
                    ));
                }
                tracing::warn!(
                    code = A11Y_TARGET_WINDOW_MINIMIZED_UIA_UNAVAILABLE,
                    hwnd,
                    error_code = error.code(),
                    error = %error,
                    "minimized target UIA snapshot failed without restoring the window"
                );
                degraded_window_input(
                    foreground,
                    mode,
                    true,
                    A11Y_TARGET_WINDOW_MINIMIZED_UIA_UNAVAILABLE,
                )
            }
        };
        populate_minimized_window_capture_probe(hwnd, &mut input);
        return Ok(input);
    }
    let tree = match synapse_a11y::snapshot_window_from_hwnd(hwnd, depth) {
        Ok(tree) => tree,
        Err(error) => {
            if error.code() == A11Y_UIA_WORKER_TIMEOUT {
                return Err(uia_worker_timeout_error(
                    &foreground,
                    "window_input_from_hwnd.snapshot",
                    &error,
                ));
            }
            tracing::warn!(
                code = A11Y_TARGET_WINDOW_SNAPSHOT_FAILED,
                hwnd,
                error_code = error.code(),
                error = %error,
                "target window UIA snapshot failed; returning degraded target metadata"
            );
            return Ok(degraded_window_input(
                foreground,
                mode,
                false,
                A11Y_TARGET_WINDOW_SNAPSHOT_FAILED,
            ));
        }
    };
    let mut input = input_from_tree_and_foreground(
        tree,
        foreground,
        mode,
        FocusSupplementScope::TargetWindow(hwnd),
    )?;
    input.is_minimized = false;
    mark_sparse_target_a11y(&mut input);
    Ok(input)
}

#[cfg(windows)]
pub fn element_input_from_id(
    element_id: &synapse_core::ElementId,
    depth: u32,
    mode: PerceptionMode,
) -> Result<ObservationInput, ErrorData> {
    let tree = synapse_a11y::snapshot_element(element_id, depth).map_err(|err| a11y_error(&err))?;
    let hwnd = element_id
        .parts()
        .map_err(|err| {
            crate::m1::mcp_error(synapse_core::error_codes::OBSERVE_INTERNAL, err.to_string())
        })?
        .hwnd;
    let foreground = windows_foreground_context(hwnd)?;
    let mut input = input_from_tree_and_foreground(
        tree,
        foreground,
        mode,
        FocusSupplementScope::TargetWindow(hwnd),
    )?;
    input.is_minimized = synapse_a11y::is_window_minimized(hwnd).unwrap_or(false);
    Ok(input)
}

#[cfg(windows)]
fn degraded_window_input(
    foreground: ForegroundContext,
    mode: PerceptionMode,
    is_minimized: bool,
    reason_code: &'static str,
) -> ObservationInput {
    let root = window_metadata_node(&foreground);
    let mut input = ObservationInput::new(foreground);
    input.is_minimized = is_minimized;
    input.focused = Some(focused_from_node(&root));
    input.elements = vec![root];
    input.a11y_status = SensorStatus::DegradedSensorFailed {
        reason_code: reason_code.to_owned(),
    };
    input.capture_status = SensorStatus::Disabled;
    input.detection_status = SensorStatus::Disabled;
    input.audio_status = SensorStatus::Disabled;
    if mode != PerceptionMode::Auto {
        input.mode_override = Some(mode);
    }
    input
}

#[cfg(windows)]
fn populate_minimized_window_capture_probe(hwnd: i64, input: &mut ObservationInput) {
    let started = Instant::now();
    let region = match synapse_capture::window_capture_region(hwnd) {
        Ok(region) => region,
        Err(error) => {
            tracing::warn!(
                code = "A11Y_TARGET_WINDOW_MINIMIZED_CAPTURE_EXTENT_UNAVAILABLE",
                hwnd,
                error_code = error.code(),
                error = %error,
                "minimized target capture extent could not be resolved"
            );
            input.capture_status = SensorStatus::DegradedSensorFailed {
                reason_code: format!("A11Y_TARGET_WINDOW_MINIMIZED_CAPTURE_{}", error.code()),
            };
            return;
        }
    };
    if region.w <= 0 || region.h <= 0 {
        input.capture_status = SensorStatus::DegradedSensorFailed {
            reason_code: "A11Y_TARGET_WINDOW_MINIMIZED_CAPTURE_EMPTY".to_owned(),
        };
        return;
    }
    match synapse_capture::window_region_to_bgra_bitmap(
        hwnd,
        region,
        MINIMIZED_WINDOW_CAPTURE_PROBE_TIMEOUT_MS,
    ) {
        Ok(captured) => {
            input.capture_status = SensorStatus::Healthy;
            input.sensor_latency_ms.insert(
                format!("capture.{}", captured.capture_backend),
                started.elapsed().as_secs_f32() * 1000.0,
            );
        }
        Err(error) => {
            tracing::warn!(
                code = "A11Y_TARGET_WINDOW_MINIMIZED_CAPTURE_UNAVAILABLE",
                hwnd,
                error_code = error.code(),
                error = %error,
                "minimized target UIA is unavailable and target-window capture probe also failed"
            );
            input.capture_status = SensorStatus::DegradedSensorFailed {
                reason_code: format!("A11Y_TARGET_WINDOW_MINIMIZED_CAPTURE_{}", error.code()),
            };
        }
    }
}

#[cfg(windows)]
fn mark_sparse_minimized_target_a11y(input: &mut ObservationInput) {
    let has_accessible_content = input
        .elements
        .iter()
        .any(|node| node.depth > 0 && !is_standard_window_chrome_node(node));
    if has_accessible_content {
        return;
    }
    tracing::warn!(
        code = A11Y_TARGET_WINDOW_MINIMIZED_UIA_UNAVAILABLE,
        hwnd = input.foreground.hwnd,
        title = %input.foreground.window_title,
        process_name = %input.foreground.process_name,
        "minimized target UIA snapshot exposed no accessible child content"
    );
    input.a11y_status = SensorStatus::DegradedSensorFailed {
        reason_code: A11Y_TARGET_WINDOW_MINIMIZED_UIA_UNAVAILABLE.to_owned(),
    };
}

#[cfg(windows)]
fn mark_sparse_target_a11y(input: &mut ObservationInput) {
    let has_accessible_content = input
        .elements
        .iter()
        .any(|node| node.depth > 0 && !is_standard_window_chrome_node(node));
    if has_accessible_content {
        return;
    }
    tracing::warn!(
        code = A11Y_TARGET_WINDOW_NO_UIA_CONTENT,
        hwnd = input.foreground.hwnd,
        title = %input.foreground.window_title,
        process_name = %input.foreground.process_name,
        "target window snapshot exposed no accessible child content"
    );
    input.a11y_status = SensorStatus::DegradedSensorFailed {
        reason_code: A11Y_TARGET_WINDOW_NO_UIA_CONTENT.to_owned(),
    };
}

#[cfg(windows)]
fn is_standard_window_chrome_node(node: &AccessibleNode) -> bool {
    match node.role.to_ascii_lowercase().as_str() {
        "title bar" | "menu bar" => true,
        "menu item" => node.name.is_empty() || node.name.eq_ignore_ascii_case("System"),
        "button" => is_standard_window_button(&node.name),
        _ => false,
    }
}

#[cfg(windows)]
fn is_standard_window_button(name: &str) -> bool {
    ["Minimize", "Maximize", "Restore", "Close", "Help"]
        .iter()
        .any(|standard| name.eq_ignore_ascii_case(standard))
}

#[cfg(windows)]
fn window_metadata_node(foreground: &ForegroundContext) -> AccessibleNode {
    AccessibleNode {
        element_id: element_id(foreground.hwnd, "0000000000000000"),
        parent: None,
        name: foreground.window_title.clone(),
        role: "Window".to_owned(),
        automation_id: None,
        value: None,
        bbox: foreground.window_bounds,
        enabled: true,
        focused: false,
        patterns: vec![UiaPattern::Window],
        children_count: 0,
        depth: 0,
    }
}

#[cfg(windows)]
fn validate_live_target_hwnd(hwnd: i64) -> Result<(), ErrorData> {
    synapse_capture::validate_hwnd(hwnd).map_err(|error| {
        crate::m1::mcp_error(
            synapse_core::error_codes::TARGET_WINDOW_NOT_FOUND,
            format!("target window_hwnd {hwnd:#x} is not a live window: {error}"),
        )
    })
}

#[cfg(windows)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FocusSupplementScope {
    GlobalForeground,
    TargetWindow(i64),
}

#[cfg(windows)]
impl FocusSupplementScope {
    const fn fallback_to_root(self) -> bool {
        matches!(self, Self::GlobalForeground)
    }
}

#[cfg(windows)]
fn input_from_tree_and_foreground(
    mut tree: synapse_core::AccessibleSubtree,
    foreground: ForegroundContext,
    mode: PerceptionMode,
    focus_scope: FocusSupplementScope,
) -> Result<ObservationInput, ErrorData> {
    let snapshot_depth = tree.max_depth;
    rebase_nodes_to_foreground(&mut tree.nodes, &foreground);
    let focused = focused_from_nodes(&tree.nodes, focus_scope);
    let mut input = ObservationInput::new(foreground);
    input.focused = focused;
    input.elements = tree.nodes;
    supplement_focused_element(&mut input, focus_scope);
    input.a11y_status = SensorStatus::Healthy;
    populate_cdp_diagnostics(&mut input);
    supplement_chromium_renderer_accessibility(&mut input, snapshot_depth);
    if mode == PerceptionMode::A11yOnly {
        input.capture_status = SensorStatus::Disabled;
    } else {
        populate_window_capture_baseline(&mut input);
    }
    if mode != PerceptionMode::Auto {
        input.mode_override = Some(mode);
    }
    Ok(input)
}

#[cfg(windows)]
fn focused_from_nodes(
    nodes: &[AccessibleNode],
    focus_scope: FocusSupplementScope,
) -> Option<FocusedElement> {
    nodes
        .iter()
        .find(|node| node.focused)
        .or_else(|| {
            focus_scope
                .fallback_to_root()
                .then(|| nodes.first())
                .flatten()
        })
        .map(focused_from_node)
}

#[cfg(windows)]
pub fn hidden_desktop_input_from_worker_snapshot(
    mut tree: synapse_core::AccessibleSubtree,
    foreground: ForegroundContext,
    mode: PerceptionMode,
) -> ObservationInput {
    rebase_nodes_to_foreground(&mut tree.nodes, &foreground);
    let focused = tree
        .nodes
        .iter()
        .find(|node| node.focused)
        .or_else(|| tree.nodes.first())
        .map(focused_from_node);
    let mut input = ObservationInput::new(foreground);
    input.focused = focused;
    input.elements = tree.nodes;
    input.a11y_status = SensorStatus::Healthy;
    input.capture_status = SensorStatus::Disabled;
    if mode != PerceptionMode::Auto {
        input.mode_override = Some(mode);
    }
    mark_sparse_target_a11y(&mut input);
    input
}

#[cfg(not(windows))]
pub fn hidden_desktop_input_from_worker_snapshot(
    _tree: synapse_core::AccessibleSubtree,
    foreground: ForegroundContext,
    mode: PerceptionMode,
) -> ObservationInput {
    let mut input = ObservationInput::new(foreground);
    input.a11y_status = SensorStatus::Unavailable;
    input.capture_status = SensorStatus::Unavailable;
    if mode != PerceptionMode::Auto {
        input.mode_override = Some(mode);
    }
    input
}

#[cfg(windows)]
fn supplement_focused_element(input: &mut ObservationInput, focus_scope: FocusSupplementScope) {
    match focus_scope {
        FocusSupplementScope::GlobalForeground => supplement_global_focused_element(input),
        FocusSupplementScope::TargetWindow(hwnd) => supplement_target_focused_element(input, hwnd),
    }
}

#[cfg(windows)]
fn supplement_global_focused_element(input: &mut ObservationInput) {
    let Ok(mut focused_node) = synapse_a11y::focused_element_node() else {
        return;
    };
    let Ok(parts) = focused_node.element_id.parts() else {
        return;
    };
    if parts.hwnd != input.foreground.hwnd {
        return;
    }
    rebase_nodes_to_foreground(std::slice::from_mut(&mut focused_node), &input.foreground);
    set_focused_node(input, focused_node);
}

#[cfg(windows)]
fn supplement_target_focused_element(input: &mut ObservationInput, hwnd: i64) {
    match synapse_a11y::focused_element_node_in_window(hwnd) {
        Ok(Some(mut focused_node)) => {
            rebase_nodes_to_foreground(std::slice::from_mut(&mut focused_node), &input.foreground);
            set_focused_node(input, focused_node);
        }
        Ok(None) => {
            input.focused = None;
        }
        Err(error) => {
            tracing::warn!(
                code = "A11Y_TARGET_FOCUSED_ELEMENT_READ_FAILED",
                hwnd,
                error_code = error.code(),
                error = %error,
                "target-window focused element readback failed"
            );
            input.focused = None;
        }
    }
}

#[cfg(windows)]
fn set_focused_node(input: &mut ObservationInput, focused_node: AccessibleNode) {
    input.focused = Some(focused_from_node(&focused_node));
    if !input
        .elements
        .iter()
        .any(|node| node.element_id == focused_node.element_id)
    {
        input.elements.push(focused_node);
    }
}

#[cfg(windows)]
fn supplement_chromium_renderer_accessibility(input: &mut ObservationInput, snapshot_depth: u32) {
    use std::collections::HashSet;

    use synapse_core::CdpStatus;

    if !synapse_a11y::is_chromium_family(&input.foreground.process_name) {
        return;
    }
    if !input
        .cdp
        .as_ref()
        .is_some_and(|cdp| cdp.status == CdpStatus::Unreachable)
    {
        return;
    }
    let started = Instant::now();
    let Ok(nodes) = synapse_a11y::chromium_renderer_accessibility_nodes_from_window(
        input.foreground.hwnd,
        snapshot_depth.max(1),
        CHROMIUM_RENDERER_UIA_SUPPLEMENT_MAX_NODES,
    ) else {
        return;
    };
    if nodes.is_empty() {
        return;
    }

    let mut seen: HashSet<_> = input
        .elements
        .iter()
        .map(|node| node.element_id.clone())
        .collect();
    let before = input.elements.len();
    input.elements.extend(
        nodes
            .into_iter()
            .filter(|node| seen.insert(node.element_id.clone())),
    );
    if input.elements.len() == before {
        return;
    }
    let foreground = input.foreground.clone();
    rebase_nodes_to_foreground(&mut input.elements, &foreground);
    input.sensor_latency_ms.insert(
        "uia_renderer".to_owned(),
        started.elapsed().as_secs_f32() * 1000.0,
    );
    tracing::info!(
        code = "A11Y_CHROMIUM_RENDERER_UIA_ATTACHED",
        hwnd = input.foreground.hwnd,
        added = input.elements.len().saturating_sub(before),
        "attached Chromium renderer UIA nodes omitted by raw child walking"
    );
}

/// CDP reachability probe timeout. Loopback connection-refused returns
/// immediately, so this only bounds the rare firewalled/dropped-port case.
#[cfg(windows)]
const CDP_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(250);

/// Surfaces the CDP probe outcome for a Chromium-family foreground into the
/// observation diagnostics. This is the #683 fix: a browser foreground always
/// carries `diagnostics.cdp.status` and a named `web_path`, so an agent that
/// gets a collapsed UIA-only tree knows *why* web content is missing and what
/// to do (open a debug port). Non-browser foregrounds leave both `None`.
///
/// The synchronous probe runs on every observe/find so both tools agree. When
/// the port is reachable the async observe/find handler later attaches CDP and
/// upgrades `web_path` to `cdp`; until DOM nodes are actually produced the path
/// stays `uia_only` to avoid claiming fidelity Synapse did not deliver.
#[cfg(windows)]
fn populate_cdp_diagnostics(input: &mut ObservationInput) {
    use synapse_core::{CdpStatus, WebPerceptionPath};

    let process_name = input.foreground.process_name.clone();
    if !synapse_a11y::is_chromium_family(&process_name) {
        return;
    }
    let started = Instant::now();
    let pid = input.foreground.pid;
    let ports = synapse_a11y::candidate_ports_for_pid(pid);
    let diagnostics =
        synapse_a11y::probe_chromium_cdp_blocking(&process_name, &ports, CDP_PROBE_TIMEOUT);
    input
        .sensor_latency_ms
        .insert("cdp".to_owned(), started.elapsed().as_secs_f32() * 1000.0);

    // Until CDP is actually attached and yields nodes, the visible tree is the
    // collapsed UIA one — report that honestly. The async handler upgrades to
    // `cdp` after a successful `getFullAXTree`.
    input.web_path = Some(WebPerceptionPath::UiaOnly);

    if diagnostics.status == CdpStatus::Unreachable {
        tracing::warn!(
            code = "A11Y_CDP_UNREACHABLE",
            process_name = %process_name,
            pid,
            probed_ports = ?ports,
            "Chromium foreground has no reachable CDP HTTP endpoint; web DOM is not \
             exposed. Launch the browser via act_launch for a dedicated debug profile, \
             set SYNAPSE_CDP_PORTS to an already-running browser that was started with \
             remote-debugging-port and a non-default user-data-dir, or use a real \
             Chrome 144+ auto-connect/native browser bridge when existing-profile \
             state is required."
        );
    }
    input.cdp = Some(diagnostics);
}

#[cfg(windows)]
fn populate_window_capture_baseline(input: &mut ObservationInput) {
    let started = Instant::now();
    let Some(probe_region) = one_pixel_region(input.foreground.window_bounds) else {
        input.capture_status = SensorStatus::DegradedSensorFailed {
            reason_code: "WINDOW_CAPTURE_EMPTY".to_owned(),
        };
        return;
    };
    match synapse_capture::screen_region_to_bgra_bitmap(probe_region) {
        Ok(_probe) => {
            input.capture_status = SensorStatus::Healthy;
            input.sensor_latency_ms.insert(
                "capture".to_owned(),
                started.elapsed().as_secs_f32() * 1000.0,
            );
            if is_luanti_foreground(&input.foreground) {
                populate_luanti_visible_baseline(input);
            }
        }
        Err(error) => {
            tracing::debug!(
                code = "OBSERVE_CAPTURE_PROBE_FAILED",
                error = %error,
                "foreground window capture probe failed"
            );
            input.capture_status = SensorStatus::DegradedSensorFailed {
                reason_code: "WINDOW_CAPTURE_FAILED".to_owned(),
            };
        }
    }
}

#[cfg(windows)]
fn populate_luanti_visible_baseline(input: &mut ObservationInput) {
    let Some(crosshair_region) = centered_region(input.foreground.window_bounds, 48, 48) else {
        return;
    };
    if let Some(reading) = contrast_reading("luanti.crosshair_contrast", crosshair_region) {
        let confidence = reading.confidence;
        input
            .hud
            .by_name
            .insert("luanti.crosshair_contrast".to_owned(), reading);
        input.entities.push(DetectedEntity {
            entity_id: entity_id(10_001),
            track_id: 10_001,
            class_label: "luanti_crosshair_region".to_owned(),
            bbox: crosshair_region,
            confidence,
            first_seen_at: Utc::now(),
            last_seen_at: Utc::now(),
            velocity_px_per_s: None,
        });
    }

    let Some(hotbar_region) = hotbar_region(input.foreground.window_bounds) else {
        return;
    };
    if let Some(reading) = contrast_reading("luanti.hotbar_contrast", hotbar_region) {
        let confidence = reading.confidence;
        input
            .hud
            .by_name
            .insert("luanti.hotbar_contrast".to_owned(), reading);
        input.entities.push(DetectedEntity {
            entity_id: entity_id(10_002),
            track_id: 10_002,
            class_label: "luanti_hotbar_region".to_owned(),
            bbox: hotbar_region,
            confidence,
            first_seen_at: Utc::now(),
            last_seen_at: Utc::now(),
            velocity_px_per_s: None,
        });
    }
}

#[cfg(windows)]
fn contrast_reading(name: &str, region: Rect) -> Option<HudReading> {
    let captured = synapse_capture::screen_region_to_bgra_bitmap(region).ok()?;
    let score = bgra_contrast_score(&captured.bytes);
    Some(HudReading {
        raw_text: format!(
            "{name} contrast={score:.3} region={}x{}@{},{}",
            captured.width, captured.height, captured.region.x, captured.region.y
        ),
        parsed: HudValue::Number(f64::from(score)),
        confidence: score.clamp(0.0, 1.0),
        stale_ms: 0,
    })
}

#[cfg(windows)]
fn bgra_contrast_score(bytes: &[u8]) -> f32 {
    let mut count = 0.0_f32;
    let mut sum = 0.0_f32;
    let mut sum_sq = 0.0_f32;
    for pixel in bytes.chunks_exact(4) {
        let b = f32::from(pixel[0]);
        let g = f32::from(pixel[1]);
        let r = f32::from(pixel[2]);
        let luma = 0.0722_f32.mul_add(b, 0.7152_f32.mul_add(g, 0.2126_f32 * r));
        count += 1.0;
        sum += luma;
        sum_sq = luma.mul_add(luma, sum_sq);
    }
    if count <= 0.0 {
        return 0.0;
    }
    let mean = sum / count;
    let variance = mean.mul_add(-mean, sum_sq / count).max(0.0);
    (variance.sqrt() / 128.0).clamp(0.0, 1.0)
}

#[cfg(windows)]
fn one_pixel_region(bounds: Rect) -> Option<Rect> {
    (bounds.w > 0 && bounds.h > 0).then_some(Rect {
        x: bounds.x,
        y: bounds.y,
        w: 1,
        h: 1,
    })
}

#[cfg(windows)]
const fn centered_region(bounds: Rect, w: i32, h: i32) -> Option<Rect> {
    if bounds.w < w || bounds.h < h || w <= 0 || h <= 0 {
        return None;
    }
    Some(Rect {
        x: bounds.x + ((bounds.w - w) / 2),
        y: bounds.y + ((bounds.h - h) / 2),
        w,
        h,
    })
}

#[cfg(windows)]
fn hotbar_region(bounds: Rect) -> Option<Rect> {
    if bounds.w <= 0 || bounds.h <= 0 {
        return None;
    }
    let w = (bounds.w / 3).clamp(180, 520).min(bounds.w);
    let h = (bounds.h / 12).clamp(48, 96).min(bounds.h);
    centered_bottom_region(bounds, w, h, 28)
}

#[cfg(windows)]
fn centered_bottom_region(bounds: Rect, w: i32, h: i32, y_offset: i32) -> Option<Rect> {
    if bounds.w < w || bounds.h < h || w <= 0 || h <= 0 {
        return None;
    }
    Some(Rect {
        x: bounds.x + ((bounds.w - w) / 2),
        y: bounds.y + bounds.h - h - y_offset.clamp(0, bounds.h - h),
        w,
        h,
    })
}

#[cfg(windows)]
fn is_luanti_foreground(foreground: &ForegroundContext) -> bool {
    foreground.process_name.eq_ignore_ascii_case("luanti.exe")
        && foreground.window_title.starts_with("Luanti ")
}

#[cfg(windows)]
fn a11y_error(err: &synapse_a11y::A11yError) -> ErrorData {
    match err {
        synapse_a11y::A11yError::NoForeground { .. }
        | synapse_a11y::A11yError::NotAvailable { .. } => crate::m1::mcp_error(
            synapse_core::error_codes::OBSERVE_NO_PERCEPTION_AVAILABLE,
            err.to_string(),
        ),
        synapse_a11y::A11yError::UiaWorkerTimeout { .. } => {
            crate::m1::mcp_error(err.code(), err.to_string())
        }
        _ => crate::m1::mcp_error(synapse_core::error_codes::OBSERVE_INTERNAL, err.to_string()),
    }
}

#[cfg(windows)]
fn uia_worker_timeout_error(
    foreground: &ForegroundContext,
    phase: &'static str,
    err: &synapse_a11y::A11yError,
) -> ErrorData {
    crate::m1::mcp_error(
        A11Y_UIA_WORKER_TIMEOUT,
        format!(
            "UIA traversal timed out phase={phase} hwnd=0x{:x} pid={} process={} title={:?} bounds={:?} detail={} remediation=retry unrelated UIA targets on the fresh worker; avoid broad UIA traversal against this target until the provider is responsive or use a background CDP/OCR path",
            foreground.hwnd,
            foreground.pid,
            foreground.process_name,
            foreground.window_title,
            foreground.window_bounds,
            err
        ),
    )
}

#[cfg(windows)]
fn focused_from_node(node: &AccessibleNode) -> FocusedElement {
    FocusedElement {
        element_id: node.element_id.clone(),
        name: node.name.clone(),
        role: node.role.clone(),
        automation_id: node.automation_id.clone(),
        bbox: node.bbox,
        enabled: node.enabled,
        patterns: node.patterns.clone(),
        value: node.value.clone(),
        selected_text: None,
    }
}

#[cfg(windows)]
fn windows_foreground_context(hwnd: i64) -> Result<ForegroundContext, ErrorData> {
    synapse_a11y::foreground_context(hwnd).map_err(|err| a11y_error(&err))
}

fn rebase_nodes_to_foreground(nodes: &mut [AccessibleNode], foreground: &ForegroundContext) {
    let Some(root) = nodes
        .iter()
        .find(|node| node.parent.is_none())
        .or_else(|| nodes.first())
    else {
        return;
    };
    let Ok(root_parts) = root.element_id.parts() else {
        return;
    };
    if root_parts.hwnd != foreground.hwnd {
        return;
    }
    let root_bbox = root.bbox;
    let foreground_bbox = foreground.window_bounds;
    if root_bbox.w <= 0
        || root_bbox.h <= 0
        || root_bbox.w != foreground_bbox.w
        || root_bbox.h != foreground_bbox.h
    {
        return;
    }
    let dx = foreground_bbox.x.saturating_sub(root_bbox.x);
    let dy = foreground_bbox.y.saturating_sub(root_bbox.y);
    if dx == 0 && dy == 0 {
        return;
    }
    tracing::debug!(
        code = "M1_A11Y_BBOX_REBASED_TO_FOREGROUND",
        hwnd = foreground.hwnd,
        dx,
        dy,
        root_x_before = root_bbox.x,
        root_y_before = root_bbox.y,
        foreground_x = foreground_bbox.x,
        foreground_y = foreground_bbox.y,
        "rebased UIA element rectangles to current foreground window position"
    );
    for node in nodes {
        if node.bbox.w > 0 && node.bbox.h > 0 {
            node.bbox.x = node.bbox.x.saturating_add(dx);
            node.bbox.y = node.bbox.y.saturating_add(dy);
        }
    }
}
