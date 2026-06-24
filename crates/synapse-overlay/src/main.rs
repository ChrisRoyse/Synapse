#![allow(unsafe_op_in_unsafe_fn)]
#![cfg_attr(not(windows), allow(unused_imports))]

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    tray::run()
}

#[cfg(not(windows))]
fn main() {
    eprintln!("synapse-overlay tray companion is currently implemented for Windows only");
}

#[cfg(windows)]
mod tray {
    use std::{
        ffi::{OsStr, c_void},
        iter::once,
        os::windows::ffi::OsStrExt,
        sync::{Arc, Mutex},
        thread,
        time::Duration,
    };

    use anyhow::{Context, Result, anyhow, bail};
    use serde::{Deserialize, Serialize};
    use windows::{
        Win32::{
            Foundation::{GetLastError, HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM},
            System::LibraryLoader::GetModuleHandleW,
            UI::{
                Shell::{
                    NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY,
                    NOTIFY_ICON_MESSAGE, NOTIFYICONDATAW, Shell_NotifyIconW, ShellExecuteW,
                },
                WindowsAndMessaging::{
                    AppendMenuW, CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, CreatePopupMenu,
                    CreateWindowExW, DefWindowProcW, DestroyMenu, DispatchMessageW, GetCursorPos,
                    GetMessageW, HICON, HMENU, IDI_APPLICATION, IMAGE_ICON, LR_DEFAULTSIZE,
                    LoadImageW, MB_ICONERROR, MB_OK, MENU_ITEM_FLAGS, MF_DISABLED, MF_GRAYED,
                    MF_SEPARATOR, MF_STRING, MSG, MessageBoxW, PostMessageW, RegisterClassW,
                    SW_SHOWNORMAL, SetForegroundWindow, TPM_BOTTOMALIGN, TPM_LEFTALIGN,
                    TrackPopupMenu, TranslateMessage, WINDOW_EX_STYLE, WM_APP, WM_COMMAND,
                    WM_CREATE, WM_DESTROY, WM_LBUTTONUP, WM_RBUTTONUP, WNDCLASSW,
                    WS_OVERLAPPEDWINDOW,
                },
            },
        },
        core::{PCWSTR, w},
    };

    const WM_TRAY: u32 = WM_APP + 0x510;
    const WM_STATUS: u32 = WM_APP + 0x511;
    const TRAY_UID: u32 = 0x5100;
    const MENU_OPEN_DASHBOARD: usize = 1001;
    const MENU_TOGGLE_RECORDING: usize = 1002;
    const MENU_REFRESH: usize = 1003;
    const MENU_QUIT: usize = 1004;
    const DEFAULT_BASE_URL: &str = "http://127.0.0.1:7700";
    const POLL_MS: u64 = 2_000;
    const HTTP_TIMEOUT_SECS: u64 = 30;

    #[derive(Clone, Debug, Eq, PartialEq)]
    enum DaemonState {
        Connected(TraySnapshot),
        Disconnected(String),
    }

    impl DaemonState {
        fn tip(&self) -> String {
            match self {
                Self::Connected(snapshot) => snapshot.tip(),
                Self::Disconnected(error) => format!("Synapse disconnected: {error}"),
            }
        }
    }

    #[derive(Clone, Debug, Eq, PartialEq, Serialize)]
    struct TraySnapshot {
        recorder_paused: bool,
        demo_armed: bool,
        pending_approvals: usize,
        active_sessions: usize,
        lease_holder: Option<String>,
        daemon_pid: Option<u32>,
    }

    impl TraySnapshot {
        fn tip(&self) -> String {
            let recorder = if self.recorder_paused {
                "paused"
            } else {
                "recording"
            };
            let demo = if self.demo_armed {
                "demo armed"
            } else {
                "demo off"
            };
            format!(
                "Synapse {recorder}; {demo}; approvals {}; sessions {}",
                self.pending_approvals, self.active_sessions
            )
        }
    }

    struct AppState {
        base_url: String,
        token: String,
        state: Mutex<DaemonState>,
    }

    pub fn run() -> Result<()> {
        if std::env::args().any(|arg| arg == "--status-once") {
            let app = app_state_from_env()?;
            let snapshot = poll_state(&app)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&snapshot).context("encode tray status")?
            );
            return Ok(());
        }
        if std::env::args().any(|arg| arg == "--help" || arg == "-h") {
            println!(
                "Usage: synapse-overlay [--status-once]\n\nStarts the Synapse Windows tray companion."
            );
            return Ok(());
        }
        let app = Arc::new(app_state_from_env()?);

        unsafe {
            let instance = GetModuleHandleW(None).context("GetModuleHandleW")?;
            let hwnd = create_window(instance.into(), Arc::clone(&app))?;
            add_or_update_icon(
                hwnd,
                &DaemonState::Disconnected("starting".to_owned()),
                NIM_ADD,
            )?;
            spawn_poll_thread(hwnd, Arc::clone(&app));
            message_loop();
            let data = notify_data(hwnd, "Synapse");
            let _ = Shell_NotifyIconW(NIM_DELETE, &data);
        }
        Ok(())
    }

    fn app_state_from_env() -> Result<AppState> {
        let base_url =
            std::env::var("SYNAPSE_TRAY_BASE_URL").unwrap_or_else(|_| DEFAULT_BASE_URL.to_owned());
        let token = std::env::var("SYNAPSE_BEARER_TOKEN")
            .context("SYNAPSE_BEARER_TOKEN must be set so the tray can use local HTTP")?;
        Ok(AppState {
            base_url,
            token,
            state: Mutex::new(DaemonState::Disconnected("starting".to_owned())),
        })
    }

    unsafe fn create_window(instance: HINSTANCE, app: Arc<AppState>) -> Result<HWND> {
        let class_name = wide("SynapseTrayCompanionWindow");
        let wc = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(window_proc),
            hInstance: instance,
            lpszClassName: PCWSTR(class_name.as_ptr()),
            ..Default::default()
        };
        if RegisterClassW(&wc) == 0 {
            bail!("RegisterClassW failed: {:?}", GetLastError());
        }
        let raw = Arc::into_raw(app);
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            PCWSTR(class_name.as_ptr()),
            w!("Synapse Tray Companion"),
            WS_OVERLAPPEDWINDOW,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            None,
            None,
            Some(instance),
            Some(raw.cast()),
        )
        .context("CreateWindowExW")?;
        if hwnd.0.is_null() {
            let _ = Arc::from_raw(raw);
            bail!("CreateWindowExW failed: {:?}", GetLastError());
        }
        Ok(hwnd)
    }

    unsafe extern "system" fn window_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        match msg {
            WM_CREATE => {
                let createstruct =
                    lparam.0 as *const windows::Win32::UI::WindowsAndMessaging::CREATESTRUCTW;
                if !createstruct.is_null() {
                    let app = (*createstruct).lpCreateParams.cast::<AppState>();
                    windows::Win32::UI::WindowsAndMessaging::SetWindowLongPtrW(
                        hwnd,
                        windows::Win32::UI::WindowsAndMessaging::GWLP_USERDATA,
                        app as isize,
                    );
                }
                LRESULT(0)
            }
            WM_TRAY => {
                let event = lparam.0 as u32;
                if (event == WM_RBUTTONUP || event == WM_LBUTTONUP)
                    && let Some(app) = app_from_window(hwnd) {
                        let _ = show_menu(hwnd, app);
                    }
                LRESULT(0)
            }
            WM_STATUS => {
                if let Some(app) = app_from_window(hwnd)
                    && let Ok(state) = app.state.lock() {
                        let _ = add_or_update_icon(hwnd, &state, NIM_MODIFY);
                    }
                LRESULT(0)
            }
            WM_COMMAND => {
                let command = wparam.0 & 0xffff;
                if let Some(app) = app_from_window(hwnd) {
                    match command {
                        MENU_OPEN_DASHBOARD => open_dashboard(app),
                        MENU_TOGGLE_RECORDING => {
                            if let Some(app) = clone_app_from_window(hwnd) {
                                thread::spawn(move || {
                                    let _ = toggle_recording(&app);
                                });
                            }
                        }
                        MENU_REFRESH => {
                            if let Some(app) = clone_app_from_window(hwnd) {
                                thread::spawn(move || {
                                    let _ = refresh_snapshot(&app);
                                });
                            }
                        }
                        MENU_QUIT => {
                            let _ = windows::Win32::UI::WindowsAndMessaging::DestroyWindow(hwnd);
                        }
                        _ => {}
                    }
                }
                LRESULT(0)
            }
            WM_DESTROY => {
                if let Some(raw) = take_app_from_window(hwnd) {
                    let _ = Arc::from_raw(raw);
                }
                windows::Win32::UI::WindowsAndMessaging::PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }

    unsafe fn app_from_window(hwnd: HWND) -> Option<&'static AppState> {
        let raw = windows::Win32::UI::WindowsAndMessaging::GetWindowLongPtrW(
            hwnd,
            windows::Win32::UI::WindowsAndMessaging::GWLP_USERDATA,
        ) as *const AppState;
        raw.as_ref()
    }

    unsafe fn clone_app_from_window(hwnd: HWND) -> Option<Arc<AppState>> {
        let raw = windows::Win32::UI::WindowsAndMessaging::GetWindowLongPtrW(
            hwnd,
            windows::Win32::UI::WindowsAndMessaging::GWLP_USERDATA,
        ) as *const AppState;
        if raw.is_null() {
            None
        } else {
            Arc::increment_strong_count(raw);
            Some(Arc::from_raw(raw))
        }
    }

    unsafe fn take_app_from_window(hwnd: HWND) -> Option<*const AppState> {
        let raw = windows::Win32::UI::WindowsAndMessaging::GetWindowLongPtrW(
            hwnd,
            windows::Win32::UI::WindowsAndMessaging::GWLP_USERDATA,
        ) as *const AppState;
        windows::Win32::UI::WindowsAndMessaging::SetWindowLongPtrW(
            hwnd,
            windows::Win32::UI::WindowsAndMessaging::GWLP_USERDATA,
            0,
        );
        (!raw.is_null()).then_some(raw)
    }

    unsafe fn message_loop() {
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }

    unsafe fn add_or_update_icon(
        hwnd: HWND,
        state: &DaemonState,
        action: NOTIFY_ICON_MESSAGE,
    ) -> Result<()> {
        let data = notify_data(hwnd, &state.tip());
        if !Shell_NotifyIconW(action, &data).as_bool() {
            bail!("Shell_NotifyIconW failed: {:?}", GetLastError());
        }
        Ok(())
    }

    unsafe fn notify_data(hwnd: HWND, tip: &str) -> NOTIFYICONDATAW {
        let mut data = NOTIFYICONDATAW {
            cbSize: u32::try_from(std::mem::size_of::<NOTIFYICONDATAW>()).unwrap_or(0),
            hWnd: hwnd,
            uID: TRAY_UID,
            uFlags: NIF_MESSAGE | NIF_TIP | NIF_ICON,
            uCallbackMessage: WM_TRAY,
            hIcon: load_icon(),
            ..Default::default()
        };
        write_wide_array(&mut data.szTip, tip);
        data
    }

    unsafe fn load_icon() -> HICON {
        let icon = LoadImageW(None, IDI_APPLICATION, IMAGE_ICON, 0, 0, LR_DEFAULTSIZE);
        icon.map_or_else(|_| HICON::default(), |handle| HICON(handle.0))
    }

    unsafe fn show_menu(hwnd: HWND, app: &AppState) -> Result<()> {
        let menu = CreatePopupMenu().context("CreatePopupMenu")?;
        let state = app
            .state
            .lock()
            .map_err(|_| anyhow!("tray state lock poisoned"))?
            .clone();
        populate_menu(menu, &state)?;
        let mut point = POINT::default();
        if let Err(error) = GetCursorPos(&mut point) {
            let _ = DestroyMenu(menu);
            bail!("GetCursorPos failed: {error}");
        }
        let _ = SetForegroundWindow(hwnd);
        let _ = TrackPopupMenu(
            menu,
            TPM_LEFTALIGN | TPM_BOTTOMALIGN,
            point.x,
            point.y,
            Some(0),
            hwnd,
            None,
        );
        let _ = DestroyMenu(menu);
        Ok(())
    }

    unsafe fn populate_menu(menu: HMENU, state: &DaemonState) -> Result<()> {
        append(menu, MENU_OPEN_DASHBOARD, "Open dashboard", MF_STRING)?;
        append(menu, 0, "", MF_SEPARATOR)?;
        match state {
            DaemonState::Connected(snapshot) => {
                append_disabled(
                    menu,
                    &format!(
                        "Recorder: {}",
                        if snapshot.recorder_paused {
                            "paused"
                        } else {
                            "recording"
                        }
                    ),
                )?;
                append_disabled(
                    menu,
                    &format!(
                        "Demo mode: {}",
                        if snapshot.demo_armed { "armed" } else { "off" }
                    ),
                )?;
                append_disabled(
                    menu,
                    &format!("Pending approvals: {}", snapshot.pending_approvals),
                )?;
                append_disabled(
                    menu,
                    &format!("Active sessions: {}", snapshot.active_sessions),
                )?;
                append_disabled(
                    menu,
                    &format!(
                        "Lease: {}",
                        snapshot.lease_holder.as_deref().unwrap_or("none")
                    ),
                )?;
                append(menu, 0, "", MF_SEPARATOR)?;
                append(
                    menu,
                    MENU_TOGGLE_RECORDING,
                    if snapshot.recorder_paused {
                        "Resume recording"
                    } else {
                        "Pause recording"
                    },
                    MF_STRING,
                )?;
            }
            DaemonState::Disconnected(error) => {
                append_disabled(menu, "Daemon: disconnected")?;
                append_disabled(menu, error)?;
            }
        }
        append(menu, MENU_REFRESH, "Refresh", MF_STRING)?;
        append(menu, 0, "", MF_SEPARATOR)?;
        append(menu, MENU_QUIT, "Quit", MF_STRING)?;
        Ok(())
    }

    unsafe fn append(menu: HMENU, id: usize, label: &str, flags: MENU_ITEM_FLAGS) -> Result<()> {
        let label = wide(label);
        AppendMenuW(menu, flags, id, PCWSTR(label.as_ptr())).context("AppendMenuW")?;
        Ok(())
    }

    unsafe fn append_disabled(menu: HMENU, label: &str) -> Result<()> {
        append(menu, 0, label, MF_STRING | MF_DISABLED | MF_GRAYED)
    }

    fn spawn_poll_thread(hwnd: HWND, app: Arc<AppState>) {
        let hwnd_raw = hwnd.0 as isize;
        thread::spawn(move || {
            loop {
                let _ = refresh_snapshot(&app);
                unsafe {
                    let hwnd = HWND(hwnd_raw as *mut c_void);
                    let _ = PostMessageW(Some(hwnd), WM_STATUS, WPARAM(0), LPARAM(0));
                }
                thread::sleep(Duration::from_millis(POLL_MS));
            }
        });
    }

    fn refresh_snapshot(app: &AppState) -> Result<()> {
        let state = match poll_state(app) {
            Ok(snapshot) => DaemonState::Connected(snapshot),
            Err(error) => DaemonState::Disconnected(format!("{error:#}")),
        };
        *app.state
            .lock()
            .map_err(|_| anyhow!("tray state lock poisoned"))? = state;
        Ok(())
    }

    fn toggle_recording(app: &AppState) -> Result<()> {
        let paused = match poll_state(app) {
            Ok(snapshot) => snapshot.recorder_paused,
            Err(error) => bail!("{error:#}"),
        };
        let endpoint = if paused {
            "/dashboard/timeline/resume"
        } else {
            "/dashboard/timeline/pause"
        };
        post_json(app, endpoint, "{}")?;
        refresh_snapshot(app)
    }

    fn open_dashboard(app: &AppState) {
        let url = wide(&format!("{}/dashboard", app.base_url.trim_end_matches('/')));
        unsafe {
            let result = ShellExecuteW(
                None,
                w!("open"),
                PCWSTR(url.as_ptr()),
                PCWSTR::null(),
                PCWSTR::null(),
                SW_SHOWNORMAL,
            );
            if result.0 as isize <= 32 {
                let message = wide("Opening the Synapse dashboard failed");
                let _ = MessageBoxW(
                    None,
                    PCWSTR(message.as_ptr()),
                    w!("Synapse"),
                    MB_OK | MB_ICONERROR,
                );
            }
        }
    }

    fn poll_state(app: &AppState) -> Result<TraySnapshot> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("build tray HTTP runtime")?;
        runtime.block_on(async {
            let client = reqwest::Client::builder()
                .timeout(Duration::from_secs(HTTP_TIMEOUT_SECS))
                .build()
                .context("build tray HTTP client")?;
            let base = app.base_url.trim_end_matches('/');
            let health_body = client
                .get(format!("{base}/health"))
                .bearer_auth(&app.token)
                .send()
                .await
                .context("GET /health")?
                .error_for_status()
                .context("/health status")?
                .text()
                .await
                .context("read /health body")?;
            let health: HealthResponse =
                serde_json::from_str(&health_body).context("decode /health")?;
            if !health.ok {
                bail!("daemon health returned ok=false");
            }
            let state = fetch_dashboard_state(&client, base, &app.token).await?;
            Ok(parse_snapshot(health, state))
        })
    }

    async fn fetch_dashboard_state(
        client: &reqwest::Client,
        base: &str,
        token: &str,
    ) -> Result<DashboardState> {
        match fetch_dashboard_text(client, base, token, "/dashboard/tray-state.json").await {
            Ok(body) => serde_json::from_str(&body).context("decode /dashboard/tray-state.json"),
            Err(tray_error) => {
                let body = fetch_dashboard_text(client, base, token, "/dashboard/state.json")
                    .await
                    .with_context(|| {
                        format!(
                            "GET /dashboard/tray-state.json failed ({tray_error:#}); fallback GET /dashboard/state.json"
                        )
                    })?;
                serde_json::from_str(&body).context("decode /dashboard/state.json")
            }
        }
    }

    async fn fetch_dashboard_text(
        client: &reqwest::Client,
        base: &str,
        token: &str,
        endpoint: &str,
    ) -> Result<String> {
        client
            .get(format!("{base}{endpoint}"))
            .bearer_auth(token)
            .send()
            .await
            .with_context(|| format!("GET {endpoint}"))?
            .error_for_status()
            .with_context(|| format!("{endpoint} status"))?
            .text()
            .await
            .with_context(|| format!("read {endpoint} body"))
    }

    fn post_json(app: &AppState, endpoint: &str, body: &'static str) -> Result<()> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("build tray HTTP runtime")?;
        runtime.block_on(async {
            let client = reqwest::Client::builder()
                .timeout(Duration::from_secs(HTTP_TIMEOUT_SECS))
                .build()
                .context("build tray HTTP client")?;
            let base = app.base_url.trim_end_matches('/');
            client
                .post(format!("{base}{endpoint}"))
                .bearer_auth(&app.token)
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(body)
                .send()
                .await
                .with_context(|| format!("POST {endpoint}"))?
                .error_for_status()
                .with_context(|| format!("{endpoint} status"))?;
            Ok(())
        })
    }

    fn parse_snapshot(health: HealthResponse, state: DashboardState) -> TraySnapshot {
        let recorder_paused = state
            .timeline
            .data
            .get("recorder")
            .and_then(|recorder| recorder.get("paused"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let pending_approvals = state
            .approvals
            .data
            .get("rows")
            .and_then(serde_json::Value::as_array)
            .map_or(0, |rows| {
                rows.iter()
                    .filter(|row| {
                        row.get("item")
                            .and_then(|item| item.get("status"))
                            .and_then(serde_json::Value::as_str)
                            == Some("pending")
                    })
                    .count()
            });
        let session_data = &state.sessions.data;
        let active_sessions = session_data
            .get("attached_agent_registry")
            .and_then(|registry| registry.get("exact_live_count"))
            .and_then(serde_json::Value::as_u64)
            .or_else(|| {
                session_data
                    .get("total_count")
                    .and_then(serde_json::Value::as_u64)
            })
            .and_then(|count| usize::try_from(count).ok())
            .unwrap_or(0);
        let lease_holder = state
            .lease
            .data
            .get("owner_session_id")
            .and_then(serde_json::Value::as_str)
            .map(ToOwned::to_owned);
        let demo_armed = state
            .demo_recording
            .data
            .get("armed")
            .and_then(serde_json::Value::as_bool)
            .or_else(|| {
                state
                    .hygiene
                    .data
                    .get("demo")
                    .and_then(|demo| demo.get("armed"))
                    .and_then(serde_json::Value::as_bool)
            })
            .unwrap_or(false);
        TraySnapshot {
            recorder_paused,
            demo_armed,
            pending_approvals,
            active_sessions,
            lease_holder,
            daemon_pid: health.pid,
        }
    }

    #[derive(Deserialize)]
    struct HealthResponse {
        ok: bool,
        pid: Option<u32>,
    }

    #[derive(Deserialize)]
    struct DashboardState {
        sessions: DashboardPanel,
        timeline: DashboardPanel,
        approvals: DashboardPanel,
        #[serde(default)]
        lease: DashboardPanel,
        #[serde(default)]
        demo_recording: DashboardPanel,
        #[serde(default)]
        hygiene: DashboardPanel,
    }

    #[derive(Default, Deserialize)]
    struct DashboardPanel {
        #[serde(default)]
        data: serde_json::Value,
    }

    fn wide(value: &str) -> Vec<u16> {
        OsStr::new(value).encode_wide().chain(once(0)).collect()
    }

    fn write_wide_array<const N: usize>(dest: &mut [u16; N], value: &str) {
        let encoded = wide(value);
        let len = encoded.len().saturating_sub(1).min(N.saturating_sub(1));
        dest[..len].copy_from_slice(&encoded[..len]);
        dest[len] = 0;
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn parse_snapshot_counts_pending_and_recorder_state() {
            let state = DashboardState {
                timeline: DashboardPanel {
                    data: serde_json::json!({
                        "recorder": {"paused": true}
                    }),
                },
                approvals: DashboardPanel {
                    data: serde_json::json!({
                        "rows": [
                            {"item": {"status": "pending"}},
                            {"item": {"status": "accepted"}},
                            {"item": {"status": "pending"}}
                        ]
                    }),
                },
                sessions: DashboardPanel {
                    data: serde_json::json!({
                        "attached_agent_registry": {"exact_live_count": 3}
                    }),
                },
                lease: DashboardPanel {
                    data: serde_json::json!({
                        "held": true,
                        "owner_session_id": "session-1"
                    }),
                },
                demo_recording: DashboardPanel {
                    data: serde_json::json!({
                        "armed": true
                    }),
                },
                hygiene: DashboardPanel {
                    data: serde_json::json!({
                        "report": {"flags": []}
                    }),
                },
            };
            let snapshot = parse_snapshot(
                HealthResponse {
                    ok: true,
                    pid: Some(12),
                },
                state,
            );
            assert!(snapshot.recorder_paused);
            assert!(snapshot.demo_armed);
            assert_eq!(snapshot.pending_approvals, 2);
            assert_eq!(snapshot.active_sessions, 3);
            assert_eq!(snapshot.lease_holder.as_deref(), Some("session-1"));
            assert_eq!(snapshot.daemon_pid, Some(12));
        }
    }
}
