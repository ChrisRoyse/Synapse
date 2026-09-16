#[cfg(windows)]
pub use platform::{
    NOTEPAD_POLL_INTERVAL, NOTEPAD_STARTUP_TIMEOUT, NOTEPAD_TITLE_REGEX, NotepadHandle,
    launch_notepad, wait_for_window_title_regex,
};
#[cfg(not(windows))]
pub use platform::{
    NOTEPAD_POLL_INTERVAL, NOTEPAD_STARTUP_TIMEOUT, NOTEPAD_TITLE_REGEX, NotepadHandle,
    launch_notepad, wait_for_window_title_regex,
};

#[cfg(any(windows, test))]
#[derive(Clone, Debug, Eq, PartialEq)]
struct WindowCandidate {
    hwnd: i64,
    pid: u32,
    title: String,
}

#[cfg(any(windows, test))]
fn select_window_title_match(
    candidates: &[WindowCandidate],
    excluded_hwnds: &std::collections::HashSet<i64>,
    preferred_pid: u32,
    title_regex: &regex::Regex,
) -> Option<WindowCandidate> {
    candidates
        .iter()
        .find(|candidate| {
            candidate.pid == preferred_pid
                && !excluded_hwnds.contains(&candidate.hwnd)
                && title_regex.is_match(&candidate.title)
        })
        .or_else(|| {
            candidates.iter().find(|candidate| {
                !excluded_hwnds.contains(&candidate.hwnd) && title_regex.is_match(&candidate.title)
            })
        })
        .cloned()
}

#[cfg(any(windows, test))]
fn select_new_notepad_window(
    candidates: &[WindowCandidate],
    excluded_hwnds: &std::collections::HashSet<i64>,
    preferred_pid: u32,
) -> Option<WindowCandidate> {
    candidates
        .iter()
        .find(|candidate| {
            candidate.pid == preferred_pid
                && !excluded_hwnds.contains(&candidate.hwnd)
                && is_notepad_window_title(&candidate.title)
        })
        .or_else(|| {
            candidates.iter().find(|candidate| {
                !excluded_hwnds.contains(&candidate.hwnd)
                    && is_notepad_window_title(&candidate.title)
            })
        })
        .cloned()
}

#[cfg(any(windows, test))]
fn is_notepad_window_title(title: &str) -> bool {
    title == "Notepad" || title.ends_with(" - Notepad")
}

#[cfg(windows)]
mod platform {
    use std::{
        collections::HashSet,
        path::PathBuf,
        process::{Child, Command, ExitStatus, Stdio},
        thread,
        time::{Duration, Instant},
    };

    use anyhow::{Context, bail};
    use regex::Regex;
    use synapse_core::ForegroundContext;

    use super::{WindowCandidate, select_new_notepad_window, select_window_title_match};

    pub const NOTEPAD_TITLE_REGEX: &str = r"^(?:Untitled - Notepad|Notepad)$";
    pub const NOTEPAD_POLL_INTERVAL: Duration = Duration::from_millis(20);
    pub const NOTEPAD_STARTUP_TIMEOUT: Duration = Duration::from_secs(5);

    const CLOSE_POLL_INTERVAL: Duration = Duration::from_millis(20);
    const GRACEFUL_CLOSE_TIMEOUT: Duration = Duration::from_secs(1);
    const FORCE_CLOSE_TIMEOUT: Duration = Duration::from_secs(2);

    pub struct NotepadHandle {
        child: Option<Child>,
        launcher_pid: u32,
        pid: u32,
        hwnd: i64,
        pid_preexisting: bool,
    }

    impl NotepadHandle {
        #[must_use]
        pub const fn pid(&self) -> u32 {
            self.pid
        }

        #[must_use]
        pub const fn hwnd(&self) -> i64 {
            self.hwnd
        }

        #[must_use]
        pub const fn pid_preexisting(&self) -> bool {
            self.pid_preexisting
        }

        pub fn current_foreground_context(&self) -> anyhow::Result<ForegroundContext> {
            synapse_a11y::foreground_context(self.hwnd).with_context(|| {
                format!("read foreground context for Notepad hwnd 0x{:x}", self.hwnd)
            })
        }

        pub fn close(mut self) -> anyhow::Result<()> {
            self.close_inner()
        }

        fn close_inner(&mut self) -> anyhow::Result<()> {
            let mut child = self.child.take();

            let close_window = synapse_a11y::close_window(self.hwnd);
            let window_closed = wait_for_window_gone(self.hwnd, GRACEFUL_CLOSE_TIMEOUT);
            let child_exited = child
                .as_mut()
                .map_or(Ok(true), |child| {
                    wait_for_child_exit(child, GRACEFUL_CLOSE_TIMEOUT)
                })
                .context("wait for Notepad launcher graceful close")?;
            if window_closed {
                if child_exited {
                    return Ok(());
                }

                let launcher_cleanup = terminate_launcher_if_safe(self.launcher_pid, self.pid);
                let child_exited = child
                    .as_mut()
                    .map_or(Ok(true), |child| {
                        wait_for_child_exit(child, FORCE_CLOSE_TIMEOUT)
                    })
                    .context("wait for Notepad launcher after window close")?;
                if child_exited {
                    return Ok(());
                }

                self.child = child;
                let close_window_status =
                    close_window.map_or_else(|err| err.to_string(), |()| "posted".to_owned());
                let launcher_cleanup_status = launcher_cleanup
                    .map_or_else(|err| err.to_string(), |status| status.to_string());
                bail!(
                    "Notepad hwnd 0x{:x} closed after WM_CLOSE ({close_window_status}) but launcher_pid {} did not exit after launcher cleanup ({launcher_cleanup_status})",
                    self.hwnd,
                    self.launcher_pid
                );
            }

            if self.pid_preexisting {
                let launcher_cleanup = terminate_launcher_if_safe(self.launcher_pid, self.pid);
                self.child = child;
                let close_window_status =
                    close_window.map_or_else(|err| err.to_string(), |()| "posted".to_owned());
                let launcher_cleanup_status = launcher_cleanup
                    .map_or_else(|err| err.to_string(), |status| status.to_string());
                bail!(
                    "Notepad hwnd 0x{:x} pid {} launcher_pid {} remained after WM_CLOSE ({close_window_status}); refusing taskkill for pre-existing Notepad pid; launcher cleanup status ({launcher_cleanup_status})",
                    self.hwnd,
                    self.pid,
                    self.launcher_pid
                );
            }

            let graceful_ui = terminate_process_tree(self.pid, false);
            let graceful_launcher = terminate_launcher_if_distinct(self.launcher_pid, self.pid);
            let window_closed = wait_for_window_gone(self.hwnd, GRACEFUL_CLOSE_TIMEOUT);
            let child_exited = child
                .as_mut()
                .map_or(Ok(true), |child| {
                    wait_for_child_exit(child, GRACEFUL_CLOSE_TIMEOUT)
                })
                .context("wait for Notepad launcher graceful taskkill close")?;
            if window_closed && child_exited {
                return Ok(());
            }

            let forced_ui = terminate_process_tree(self.pid, true);
            let forced_launcher = terminate_launcher_if_distinct_force(self.launcher_pid, self.pid);
            let window_closed = wait_for_window_gone(self.hwnd, FORCE_CLOSE_TIMEOUT);
            let child_exited = child
                .as_mut()
                .map_or(Ok(true), |child| {
                    wait_for_child_exit(child, FORCE_CLOSE_TIMEOUT)
                })
                .context("wait for Notepad launcher forced close")?;
            if window_closed && child_exited {
                return Ok(());
            }

            self.child = child;
            let graceful_ui_status =
                graceful_ui.map_or_else(|err| err.to_string(), |status| status.to_string());
            let graceful_launcher_status =
                graceful_launcher.map_or_else(|err| err.to_string(), |status| status.to_string());
            let forced_ui_status =
                forced_ui.map_or_else(|err| err.to_string(), |status| status.to_string());
            let forced_launcher_status =
                forced_launcher.map_or_else(|err| err.to_string(), |status| status.to_string());
            bail!(
                "Notepad hwnd 0x{:x} pid {} launcher_pid {} remained after taskkill /T ui ({graceful_ui_status}), taskkill /T launcher ({graceful_launcher_status}), taskkill /T /F ui ({forced_ui_status}), and taskkill /T /F launcher ({forced_launcher_status})",
                self.hwnd,
                self.pid,
                self.launcher_pid
            );
        }
    }

    impl Drop for NotepadHandle {
        fn drop(&mut self) {
            let _ = self.close_inner();
        }
    }

    #[allow(clippy::trivial_regex)]
    pub fn launch_notepad() -> anyhow::Result<NotepadHandle> {
        let title_regex = Regex::new(NOTEPAD_TITLE_REGEX).context("compile Notepad title regex")?;
        let existing_windows = visible_top_level_windows()
            .context("snapshot existing top-level windows before Notepad launch")?;
        let excluded_hwnds = matching_window_hwnds(&existing_windows, &title_regex);
        let excluded_notepad_hwnds = notepad_window_hwnds(&existing_windows);
        let existing_notepad_pids =
            notepad_process_ids().context("snapshot existing notepad.exe pids before launch")?;
        let mut child = Command::new(notepad_exe())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("spawn notepad.exe")?;
        let pid = child.id();
        if pid == 0 {
            let _ = child.kill();
            let _ = child.wait();
            bail!("spawned notepad.exe without a process id");
        }

        // On Win11 22H2+ packaged Notepad with session restore, a fresh
        // `notepad.exe` launch may reopen the last-used document tab and never
        // produce a blank Notepad window. If the initial wait times out, target
        // the newly observed Notepad HWND, send Ctrl+N once, then retry.
        let context = match wait_for_new_window_title_regex(pid, &title_regex, &excluded_hwnds) {
            Ok(context) => context,
            Err(first_err) => match force_untitled_notepad_tab(
                pid,
                &excluded_notepad_hwnds,
                &first_err,
            )
            .and_then(|candidate| {
                wait_for_new_window_title_regex(pid, &title_regex, &excluded_hwnds).with_context(
                    || {
                        format!(
                            "Notepad did not reach the expected startup title after Ctrl+N retry against hwnd 0x{:x} pid {}; first_timeout={first_err:#}",
                            candidate.hwnd, candidate.pid
                        )
                    },
                )
            }) {
                Ok(context) => context,
                Err(err) => {
                    let _ = terminate_process_tree(pid, true);
                    let _ = wait_for_child_exit(&mut child, FORCE_CLOSE_TIMEOUT);
                    return Err(err).context("Notepad did not reach the expected startup title");
                }
            },
        };
        let pid_preexisting = existing_notepad_pids.contains(&context.pid);

        Ok(NotepadHandle {
            child: Some(child),
            launcher_pid: pid,
            pid: context.pid,
            hwnd: context.hwnd,
            pid_preexisting,
        })
    }

    pub fn wait_for_window_title_regex(
        pid: u32,
        title_regex: &Regex,
    ) -> anyhow::Result<ForegroundContext> {
        if pid == 0 {
            bail!("Notepad pid must be non-zero");
        }
        wait_for_new_window_title_regex(pid, title_regex, &HashSet::new())
    }

    fn wait_for_new_window_title_regex(
        pid: u32,
        title_regex: &Regex,
        excluded_hwnds: &HashSet<i64>,
    ) -> anyhow::Result<ForegroundContext> {
        let start = Instant::now();
        let mut last_candidates: Vec<WindowCandidate> = Vec::new();
        let mut last_error: Option<String> = None;

        while start.elapsed() <= NOTEPAD_STARTUP_TIMEOUT {
            match visible_top_level_windows() {
                Ok(candidates) => {
                    if let Some(candidate) =
                        select_window_title_match(&candidates, excluded_hwnds, pid, title_regex)
                    {
                        return context_for_window(candidate.hwnd);
                    }
                    last_candidates = candidates;
                    last_error = None;
                }
                Err(err) => {
                    last_error = Some(err.to_string());
                }
            }
            thread::sleep(NOTEPAD_POLL_INTERVAL);
        }

        bail!(
            "timed out after {:?} waiting for launcher pid {pid} or a new visible top-level window title to match {}; excluded_hwnds={excluded_hwnds:?}; last_candidates={last_candidates:?}; last_error={last_error:?}",
            NOTEPAD_STARTUP_TIMEOUT,
            title_regex.as_str()
        );
    }

    fn context_for_window(hwnd: i64) -> anyhow::Result<ForegroundContext> {
        synapse_a11y::foreground_context(hwnd)
            .with_context(|| format!("read foreground context for hwnd 0x{hwnd:x}"))
    }

    fn matching_window_hwnds(windows: &[WindowCandidate], title_regex: &Regex) -> HashSet<i64> {
        windows
            .iter()
            .filter(|candidate| title_regex.is_match(&candidate.title))
            .map(|candidate| candidate.hwnd)
            .collect()
    }

    fn notepad_window_hwnds(windows: &[WindowCandidate]) -> HashSet<i64> {
        windows
            .iter()
            .filter(|candidate| super::is_notepad_window_title(&candidate.title))
            .map(|candidate| candidate.hwnd)
            .collect()
    }

    fn force_untitled_notepad_tab(
        preferred_pid: u32,
        excluded_notepad_hwnds: &HashSet<i64>,
        first_err: &anyhow::Error,
    ) -> anyhow::Result<WindowCandidate> {
        let candidates = visible_top_level_windows().with_context(|| {
            format!("snapshot Notepad windows after first timeout: {first_err:#}")
        })?;
        let candidate = select_new_notepad_window(
            &candidates,
            excluded_notepad_hwnds,
            preferred_pid,
        )
        .with_context(|| {
            format!(
                "find newly launched/restored Notepad window for Ctrl+N retry after first timeout: {first_err:#}; excluded_notepad_hwnds={excluded_notepad_hwnds:?}; candidates={candidates:?}"
            )
        })?;
        synapse_a11y::focus_window_with_intent(
            candidate.hwnd,
            synapse_a11y::ForegroundActivationIntent::OperatorRequested {
                caller: "synapse_test_utils_force_untitled_notepad_tab",
            },
        )
        .with_context(|| {
            format!(
                "focus restored Notepad window hwnd 0x{:x} pid {} before Ctrl+N retry",
                candidate.hwnd, candidate.pid
            )
        })?;
        send_ctrl_n_to_foreground_notepad().with_context(|| {
            format!(
                "send Ctrl+N to restored Notepad window hwnd 0x{:x} pid {}",
                candidate.hwnd, candidate.pid
            )
        })?;
        Ok(candidate)
    }

    #[cfg(test)]
    fn wait_for_new_notepad_window(
        preferred_pid: u32,
        excluded_notepad_hwnds: &HashSet<i64>,
        timeout: Duration,
    ) -> anyhow::Result<WindowCandidate> {
        let start = Instant::now();
        let mut last_candidates = Vec::new();
        let mut last_error = None;
        while start.elapsed() <= timeout {
            match visible_top_level_windows() {
                Ok(candidates) => {
                    if let Some(candidate) = select_new_notepad_window(
                        &candidates,
                        excluded_notepad_hwnds,
                        preferred_pid,
                    ) {
                        return Ok(candidate);
                    }
                    last_candidates = candidates;
                    last_error = None;
                }
                Err(err) => last_error = Some(err.to_string()),
            }
            thread::sleep(NOTEPAD_POLL_INTERVAL);
        }
        bail!(
            "timed out after {timeout:?} waiting for a new Notepad window; preferred_pid={preferred_pid}; excluded_notepad_hwnds={excluded_notepad_hwnds:?}; last_candidates={last_candidates:?}; last_error={last_error:?}"
        );
    }

    fn send_ctrl_n_to_foreground_notepad() -> anyhow::Result<()> {
        let command = "$shell = New-Object -ComObject WScript.Shell; Start-Sleep -Milliseconds 100; $shell.SendKeys('^n'); exit 0";
        let output = Command::new("powershell.exe")
            .args(["-NoProfile", "-Command", command])
            .stdin(Stdio::null())
            .output()
            .context("run PowerShell WScript.Shell SendKeys Ctrl+N")?;
        if !output.status.success() {
            bail!(
                "PowerShell SendKeys Ctrl+N failed with status {} stdout={} stderr={}",
                output.status,
                String::from_utf8_lossy(&output.stdout).trim(),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(())
    }

    fn notepad_process_ids() -> anyhow::Result<HashSet<u32>> {
        // PowerShell 5.1 (powershell.exe) sets $LASTEXITCODE=1 when
        // `Get-Process notepad -EA SilentlyContinue` finds nothing, even though
        // SilentlyContinue suppresses the error text. Explicit `exit 0` makes
        // an empty-snapshot a valid success.
        let output = Command::new("powershell.exe")
            .args([
                "-NoProfile",
                "-Command",
                "Get-Process notepad -ErrorAction SilentlyContinue | ForEach-Object { [string]$_.Id }; exit 0",
            ])
            .stdin(Stdio::null())
            .output()
            .context("run PowerShell notepad process id snapshot")?;
        if !output.status.success() {
            bail!(
                "PowerShell notepad process id snapshot failed with status {} stderr={}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }

        output
            .stdout
            .split(|byte| *byte == b'\n' || *byte == b'\r')
            .filter_map(|line| {
                let line = std::str::from_utf8(line).ok()?.trim();
                if line.is_empty() {
                    None
                } else {
                    Some(line.parse::<u32>())
                }
            })
            .collect::<Result<HashSet<_>, _>>()
            .context("parse existing notepad.exe pids")
    }

    fn visible_top_level_windows() -> anyhow::Result<Vec<WindowCandidate>> {
        Ok(synapse_a11y::visible_top_level_window_contexts()?
            .into_iter()
            .map(|context| WindowCandidate {
                hwnd: context.hwnd,
                pid: context.pid,
                title: context.window_title,
            })
            .collect())
    }

    fn wait_for_window_gone(hwnd: i64, timeout: Duration) -> bool {
        let start = Instant::now();
        while start.elapsed() <= timeout {
            if synapse_a11y::foreground_context(hwnd).is_err() {
                return true;
            }
            thread::sleep(CLOSE_POLL_INTERVAL);
        }
        false
    }

    fn notepad_exe() -> PathBuf {
        if let Some(system_root) = std::env::var_os("SystemRoot") {
            let candidate = PathBuf::from(system_root)
                .join("System32")
                .join("notepad.exe");
            if candidate.exists() {
                return candidate;
            }
        }
        PathBuf::from("notepad.exe")
    }

    fn terminate_process_tree(pid: u32, force: bool) -> anyhow::Result<ExitStatus> {
        let pid_arg = pid.to_string();
        let mut command = Command::new("taskkill");
        command
            .args(["/PID", &pid_arg, "/T"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if force {
            command.arg("/F");
        }
        let status = command.status().context("run taskkill")?;
        if force
            && !status.success()
            && let Ok(cim_status) = terminate_process_via_cim(pid)
        {
            return Ok(cim_status);
        }
        Ok(status)
    }

    fn terminate_process_via_cim(pid: u32) -> anyhow::Result<ExitStatus> {
        let command_text = format!(
            "Get-CimInstance Win32_Process -Filter \"ProcessId={pid}\" | ForEach-Object {{ Invoke-CimMethod -InputObject $_ -MethodName Terminate | Out-Null }}"
        );
        Command::new("powershell.exe")
            .args(["-NoProfile", "-Command", &command_text])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .context("run PowerShell Win32_Process.Terminate fallback")
    }

    fn terminate_launcher_if_safe(launcher_pid: u32, ui_pid: u32) -> anyhow::Result<ExitStatus> {
        if launcher_pid == ui_pid {
            bail!("launcher pid matches UI pid; refusing launcher-only taskkill");
        }
        terminate_process_tree(launcher_pid, false)
    }

    fn terminate_launcher_if_distinct(
        launcher_pid: u32,
        ui_pid: u32,
    ) -> anyhow::Result<ExitStatus> {
        if launcher_pid == ui_pid {
            return terminate_process_tree(ui_pid, false);
        }
        terminate_process_tree(launcher_pid, false)
    }

    fn terminate_launcher_if_distinct_force(
        launcher_pid: u32,
        ui_pid: u32,
    ) -> anyhow::Result<ExitStatus> {
        if launcher_pid == ui_pid {
            return terminate_process_tree(ui_pid, true);
        }
        terminate_process_tree(launcher_pid, true)
    }

    fn wait_for_child_exit(child: &mut Child, timeout: Duration) -> anyhow::Result<bool> {
        let start = Instant::now();
        while start.elapsed() <= timeout {
            if child
                .try_wait()
                .context("check child process status")?
                .is_some()
            {
                return Ok(true);
            }
            thread::sleep(CLOSE_POLL_INTERVAL);
        }
        Ok(false)
    }

    #[cfg(test)]
    mod tests {
        use std::{
            io::Write,
            process::{Command, Stdio},
        };

        use anyhow::Context;
        use regex::Regex;
        use tempfile::NamedTempFile;

        use super::{
            FORCE_CLOSE_TIMEOUT, NOTEPAD_STARTUP_TIMEOUT, NOTEPAD_TITLE_REGEX, context_for_window,
            force_untitled_notepad_tab, matching_window_hwnds, notepad_exe, notepad_window_hwnds,
            terminate_process_tree, visible_top_level_windows, wait_for_child_exit,
            wait_for_new_notepad_window, wait_for_new_window_title_regex, wait_for_window_gone,
        };

        #[test]
        #[ignore = "requires an interactive Windows desktop with Notepad and UIA"]
        #[allow(clippy::trivial_regex)]
        fn ctrl_n_retry_converts_new_saved_notepad_window_to_blank_notepad() -> anyhow::Result<()> {
            let mut file =
                NamedTempFile::new().context("create temp Notepad restore probe file")?;
            writeln!(file, "synapse issue 238 restore probe")
                .context("write temp Notepad restore probe file")?;
            let file_path = file.path().to_owned();
            let title_regex =
                Regex::new(NOTEPAD_TITLE_REGEX).context("compile Notepad title regex")?;
            let existing_windows = visible_top_level_windows()
                .context("snapshot windows before saved-tab Notepad launch")?;
            let excluded_exact_hwnds = matching_window_hwnds(&existing_windows, &title_regex);
            let excluded_notepad_hwnds = notepad_window_hwnds(&existing_windows);
            println!(
                "regression_check=notepad_ctrl_n_retry edge=saved_tab before excluded_notepad_hwnds={excluded_notepad_hwnds:?} file={}",
                file_path.display()
            );

            let mut child = Command::new(notepad_exe())
                .arg(&file_path)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .context("spawn Notepad with saved restore probe file")?;
            let launcher_pid = child.id();
            let saved_candidate = wait_for_new_notepad_window(
                launcher_pid,
                &excluded_notepad_hwnds,
                NOTEPAD_STARTUP_TIMEOUT,
            )
            .context("wait for saved-tab Notepad candidate")?;
            let saved_context = context_for_window(saved_candidate.hwnd)
                .context("read saved-tab Notepad context")?;
            println!(
                "regression_check=notepad_ctrl_n_retry edge=saved_tab before_retry hwnd=0x{:x} pid={} title={:?}",
                saved_context.hwnd, saved_context.pid, saved_context.window_title
            );

            let retry_candidate = force_untitled_notepad_tab(
                launcher_pid,
                &excluded_notepad_hwnds,
                &anyhow::anyhow!("synthetic saved-tab retry probe"),
            )
            .context("force Ctrl+N retry for saved-tab Notepad candidate")?;
            let untitled_context =
                wait_for_new_window_title_regex(launcher_pid, &title_regex, &excluded_exact_hwnds)
                    .context("wait for Ctrl+N retry to produce Untitled Notepad")?;
            println!(
                "regression_check=notepad_ctrl_n_retry edge=saved_tab after_retry source_hwnd=0x{:x} source_pid={} hwnd=0x{:x} pid={} title={:?}",
                retry_candidate.hwnd,
                retry_candidate.pid,
                untitled_context.hwnd,
                untitled_context.pid,
                untitled_context.window_title
            );
            assert!(
                super::super::is_notepad_window_title(&untitled_context.window_title),
                "expected blank Notepad title, got {:?}",
                untitled_context.window_title
            );

            synapse_a11y::close_window(untitled_context.hwnd)
                .context("close Ctrl+N retry Notepad window")?;
            let mut cleanup_stage = "wm_close";
            let mut closed = wait_for_window_gone(untitled_context.hwnd, FORCE_CLOSE_TIMEOUT);
            if !closed {
                cleanup_stage = "graceful_taskkill_ui";
                let _ = terminate_process_tree(untitled_context.pid, false);
                if launcher_pid != untitled_context.pid {
                    let _ = terminate_process_tree(launcher_pid, false);
                }
                closed = wait_for_window_gone(untitled_context.hwnd, FORCE_CLOSE_TIMEOUT);
            }
            if !closed {
                cleanup_stage = "forced_taskkill_ui";
                let _ = terminate_process_tree(untitled_context.pid, true);
                if launcher_pid != untitled_context.pid {
                    let _ = terminate_process_tree(launcher_pid, true);
                }
                closed = wait_for_window_gone(untitled_context.hwnd, FORCE_CLOSE_TIMEOUT);
            }
            let child_exited = wait_for_child_exit(&mut child, FORCE_CLOSE_TIMEOUT)
                .context("wait for saved-tab Notepad launcher after cleanup")?;
            println!(
                "regression_check=notepad_ctrl_n_retry edge=saved_tab after_close hwnd=0x{:x} ui_pid={} launcher_pid={} closed={closed} cleanup_stage={cleanup_stage} child_exited={child_exited}",
                untitled_context.hwnd, untitled_context.pid, launcher_pid
            );
            assert!(closed);
            assert!(child_exited || launcher_pid != untitled_context.pid);
            Ok(())
        }
    }
}

#[cfg(not(windows))]
mod platform {
    use std::time::Duration;

    use anyhow::bail;
    use regex::Regex;
    use synapse_core::ForegroundContext;

    pub const NOTEPAD_TITLE_REGEX: &str = r"^(?:Untitled - Notepad|Notepad)$";
    pub const NOTEPAD_POLL_INTERVAL: Duration = Duration::from_millis(20);
    pub const NOTEPAD_STARTUP_TIMEOUT: Duration = Duration::from_secs(5);

    pub struct NotepadHandle {
        _private: (),
    }

    impl NotepadHandle {
        #[must_use]
        pub const fn pid(&self) -> u32 {
            0
        }

        #[must_use]
        pub const fn hwnd(&self) -> i64 {
            0
        }

        #[must_use]
        pub const fn pid_preexisting(&self) -> bool {
            false
        }

        pub fn current_foreground_context(&self) -> anyhow::Result<ForegroundContext> {
            bail!("launch_notepad foreground readback requires Windows UI Automation");
        }

        pub const fn close(self) -> anyhow::Result<()> {
            Ok(())
        }
    }

    pub fn launch_notepad() -> anyhow::Result<NotepadHandle> {
        bail!("launch_notepad requires Windows");
    }

    pub fn wait_for_window_title_regex(
        _pid: u32,
        _title_regex: &Regex,
    ) -> anyhow::Result<ForegroundContext> {
        bail!("wait_for_window_title_regex requires Windows");
    }
}

#[cfg(test)]
mod tests {
    use regex::Regex;

    #[cfg(not(windows))]
    use super::wait_for_window_title_regex;
    use super::{
        NOTEPAD_POLL_INTERVAL, NOTEPAD_STARTUP_TIMEOUT, NOTEPAD_TITLE_REGEX, launch_notepad,
    };
    #[cfg(any(windows, test))]
    use super::{
        WindowCandidate, is_notepad_window_title, select_new_notepad_window,
        select_window_title_match,
    };

    #[test]
    fn notepad_fixture_constants_match_m2_contract() {
        println!(
            "readback=fixtures_constants before=expected_poll_ms:20 expected_timeout_s:5 expected_regex:{NOTEPAD_TITLE_REGEX:?}"
        );
        assert_eq!(NOTEPAD_POLL_INTERVAL.as_millis(), 20);
        assert_eq!(NOTEPAD_STARTUP_TIMEOUT.as_secs(), 5);
        assert_eq!(NOTEPAD_TITLE_REGEX, r"^(?:Untitled - Notepad|Notepad)$");
        println!(
            "readback=fixtures_constants after=poll_ms:{} timeout_s:{} regex:{NOTEPAD_TITLE_REGEX:?}",
            NOTEPAD_POLL_INTERVAL.as_millis(),
            NOTEPAD_STARTUP_TIMEOUT.as_secs()
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn launch_notepad_fails_closed_off_windows() {
        println!("readback=launch_notepad_non_windows before=handle_present:false");
        let error = match launch_notepad() {
            Ok(_handle) => panic!("launch_notepad unexpectedly succeeded off Windows"),
            Err(error) => error,
        };
        println!("readback=launch_notepad_non_windows after=error:{error}");
        assert!(error.to_string().contains("requires Windows"));
    }

    #[cfg(not(windows))]
    #[allow(clippy::trivial_regex)]
    #[test]
    fn wait_for_window_title_regex_fails_closed_off_windows() {
        let regex = match Regex::new(NOTEPAD_TITLE_REGEX) {
            Ok(regex) => regex,
            Err(error) => panic!("failed to compile Notepad title regex: {error}"),
        };
        println!(
            "readback=wait_for_window_title_regex_non_windows before=pid:1 regex:{:?}",
            regex.as_str()
        );
        let error = match wait_for_window_title_regex(1, &regex) {
            Ok(_context) => {
                panic!("wait_for_window_title_regex unexpectedly succeeded off Windows")
            }
            Err(error) => error,
        };
        println!("readback=wait_for_window_title_regex_non_windows after=error:{error}");
        assert!(error.to_string().contains("requires Windows"));
    }

    #[allow(clippy::trivial_regex)]
    #[test]
    fn notepad_window_selection_prefers_launcher_pid_and_skips_preexisting() {
        let regex = match Regex::new(NOTEPAD_TITLE_REGEX) {
            Ok(regex) => regex,
            Err(error) => panic!("failed to compile Notepad title regex: {error}"),
        };
        let mut excluded = std::collections::HashSet::new();
        excluded.insert(10);
        let candidates = vec![
            WindowCandidate {
                hwnd: 10,
                pid: 900,
                title: "Untitled - Notepad".to_owned(),
            },
            WindowCandidate {
                hwnd: 11,
                pid: 123,
                title: "Untitled - Notepad".to_owned(),
            },
            WindowCandidate {
                hwnd: 12,
                pid: 456,
                title: "Untitled - Notepad".to_owned(),
            },
        ];

        println!(
            "regression_check=notepad_window_selection edge=prefer_launcher before=preferred_pid:123 excluded={excluded:?} candidates={candidates:?}"
        );
        let selected = select_window_title_match(&candidates, &excluded, 123, &regex);
        println!(
            "regression_check=notepad_window_selection edge=prefer_launcher after={selected:?} expected_hwnd=11"
        );
        assert_eq!(selected.map(|candidate| candidate.hwnd), Some(11));
    }

    #[allow(clippy::trivial_regex)]
    #[test]
    fn notepad_window_selection_accepts_packaged_notepad_different_pid() {
        let regex = match Regex::new(NOTEPAD_TITLE_REGEX) {
            Ok(regex) => regex,
            Err(error) => panic!("failed to compile Notepad title regex: {error}"),
        };
        let excluded = std::collections::HashSet::new();
        let candidates = vec![WindowCandidate {
            hwnd: 44,
            pid: 9001,
            title: "Notepad".to_owned(),
        }];

        println!(
            "regression_check=notepad_window_selection edge=packaged_pid_transfer before=preferred_pid:123 excluded={excluded:?} candidates={candidates:?}"
        );
        let selected = select_window_title_match(&candidates, &excluded, 123, &regex);
        println!(
            "regression_check=notepad_window_selection edge=packaged_pid_transfer after={selected:?} expected_hwnd=44"
        );
        assert_eq!(selected.map(|candidate| candidate.hwnd), Some(44));
    }

    #[allow(clippy::trivial_regex)]
    #[test]
    fn notepad_window_selection_rejects_excluded_or_wrong_title() {
        let regex = match Regex::new(NOTEPAD_TITLE_REGEX) {
            Ok(regex) => regex,
            Err(error) => panic!("failed to compile Notepad title regex: {error}"),
        };
        let mut excluded = std::collections::HashSet::new();
        excluded.insert(70);
        let candidates = vec![
            WindowCandidate {
                hwnd: 70,
                pid: 123,
                title: "Untitled - Notepad".to_owned(),
            },
            WindowCandidate {
                hwnd: 71,
                pid: 123,
                title: "Settings".to_owned(),
            },
        ];

        println!(
            "regression_check=notepad_window_selection edge=no_match before=preferred_pid:123 excluded={excluded:?} candidates={candidates:?}"
        );
        let selected = select_window_title_match(&candidates, &excluded, 123, &regex);
        println!(
            "regression_check=notepad_window_selection edge=no_match after={selected:?} expected=None"
        );
        assert_eq!(selected, None);
    }

    #[test]
    fn notepad_restore_retry_selects_only_new_notepad_windows() {
        let mut excluded = std::collections::HashSet::new();
        excluded.insert(70);
        let candidates = vec![
            WindowCandidate {
                hwnd: 70,
                pid: 5000,
                title: "api.txt - Notepad".to_owned(),
            },
            WindowCandidate {
                hwnd: 71,
                pid: 6000,
                title: "m2-demo.txt - Notepad".to_owned(),
            },
            WindowCandidate {
                hwnd: 72,
                pid: 6000,
                title: "Settings".to_owned(),
            },
        ];

        println!(
            "regression_check=notepad_restore_retry edge=new_saved_tab before=preferred_pid:6000 excluded={excluded:?} candidates={candidates:?}"
        );
        let selected = select_new_notepad_window(&candidates, &excluded, 6000);
        println!(
            "regression_check=notepad_restore_retry edge=new_saved_tab after={selected:?} expected_hwnd=71"
        );
        assert_eq!(selected.map(|candidate| candidate.hwnd), Some(71));
    }

    #[test]
    fn notepad_restore_retry_rejects_only_preexisting_notepad_windows() {
        let mut excluded = std::collections::HashSet::new();
        excluded.insert(70);
        let candidates = vec![
            WindowCandidate {
                hwnd: 70,
                pid: 5000,
                title: "api.txt - Notepad".to_owned(),
            },
            WindowCandidate {
                hwnd: 71,
                pid: 6000,
                title: "Settings".to_owned(),
            },
        ];

        println!(
            "regression_check=notepad_restore_retry edge=preexisting_only before=preferred_pid:6000 excluded={excluded:?} candidates={candidates:?}"
        );
        let selected = select_new_notepad_window(&candidates, &excluded, 6000);
        println!(
            "regression_check=notepad_restore_retry edge=preexisting_only after={selected:?} expected=None"
        );
        assert_eq!(selected, None);
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "requires an interactive Windows desktop with Notepad and UIA"]
    fn launch_notepad_reaches_expected_window_title_on_windows() -> anyhow::Result<()> {
        println!(
            "readback=synapse_a11y::foreground_context(hwnd).window_title before=notepad_absent_or_unobserved"
        );
        let handle = launch_notepad()?;
        let context = handle.current_foreground_context()?;
        println!(
            "readback=synapse_a11y::foreground_context hwnd={} pid={} after_title={:?}",
            handle.hwnd(),
            handle.pid(),
            context.window_title
        );
        assert_eq!(context.pid, handle.pid());
        assert_eq!(context.hwnd, handle.hwnd());
        assert!(
            is_notepad_window_title(&context.window_title),
            "expected blank Notepad title, got {:?}",
            context.window_title
        );

        let pid = handle.pid();
        handle.close()?;
        println!("readback=NotepadHandle::close after=closed pid={pid}");
        Ok(())
    }
}
