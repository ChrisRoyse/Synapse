//! Unit tests for m4 (split out of the module body per #1555, 6/6).

use super::*;

#[cfg(windows)]
#[test]
fn shell_search_tool_readback_resolves_windows_builtins() {
    // The readback resolves against the same child-process environment shell
    // jobs receive. `findstr` and `powershell` are Windows built-ins that
    // `ensure_windows_path_entries` always merges into the child PATH, so
    // they must resolve to real files — otherwise the readback (and the
    // shell env it describes) is broken. This is the FSV anchor: a missing
    // findstr here means the child PATH is not actually usable.
    let readback = shell_search_tool_readback();
    assert!(
        readback.starts_with("shell_search_tools "),
        "unexpected readback shape: {readback}"
    );
    assert!(
        readback.contains("documented_fallback=powershell_select_string"),
        "readback must name the documented fallback primitive: {readback}"
    );
    assert!(
        !readback.contains("findstr=absent"),
        "findstr is a Windows built-in on the child PATH and must resolve: {readback}"
    );
    assert!(
        !readback.contains("powershell=absent"),
        "powershell must resolve on the child PATH: {readback}"
    );
    // `primary` must never claim a tool the readback reports as absent.
    let primary = readback
        .split_whitespace()
        .find_map(|token| token.strip_prefix("primary="))
        .expect("readback must include a primary= token");
    assert!(
        matches!(primary, "rg" | "findstr" | "powershell_select_string"),
        "unexpected primary tool {primary}: {readback}"
    );
    if primary == "rg" {
        assert!(
            !readback.contains("rg=absent"),
            "primary=rg contradicts rg=absent: {readback}"
        );
    }
}

#[test]
fn reserved_variable_assignment_detection() {
    // Collisions that must be refused.
    assert_eq!(
        detect_shell_reserved_variable_assignment("$home = \"C:\\temp\\calyx\""),
        Some("home")
    );
    assert_eq!(
        detect_shell_reserved_variable_assignment("$HOME=$scratch"),
        Some("home")
    );
    assert_eq!(
        detect_shell_reserved_variable_assignment("$Profile = 'x'"),
        Some("profile")
    );
    assert_eq!(
        detect_shell_reserved_variable_assignment("$PWD += 'x'"),
        Some("pwd")
    );
    // Safe: read-only use, env namespace, different identifier, RHS use.
    assert_eq!(
        detect_shell_reserved_variable_assignment("Join-Path $HOME 'sub'"),
        None
    );
    assert_eq!(
        detect_shell_reserved_variable_assignment("$env:HOME = 'x'"),
        None
    );
    assert_eq!(
        detect_shell_reserved_variable_assignment("$homedir = 'x'"),
        None
    );
    assert_eq!(
        detect_shell_reserved_variable_assignment("$scratch = $HOME"),
        None
    );
    assert_eq!(
        detect_shell_reserved_variable_assignment("if ($host -eq 'x') { 1 }"),
        None
    );
}

#[test]
fn uncontained_recursive_delete_detection() {
    // The exact #1507 shape and variants must be refused.
    assert_eq!(
        detect_uncontained_recursive_delete("Remove-Item $home -Recurse -Force"),
        Some("$home")
    );
    assert_eq!(
        detect_uncontained_recursive_delete("rm -r $env:USERPROFILE\\scratch"),
        Some("$env:userprofile")
    );
    assert_eq!(
        detect_uncontained_recursive_delete("[System.IO.Directory]::Delete($profile, $true)"),
        Some("$profile")
    );
    // Safe: recursive delete of an explicit workspace path (no home ref).
    assert_eq!(
        detect_uncontained_recursive_delete(
            "Remove-Item C:\\code\\Synapse\\target\\fsv -Recurse -Force"
        ),
        None
    );
    // Safe: reference present but NOT recursive.
    assert_eq!(detect_uncontained_recursive_delete("Get-Item $home"), None);
    assert_eq!(
        detect_uncontained_recursive_delete("Remove-Item $home"),
        None
    );
}

#[test]
fn validate_run_shell_params_refuses_reserved_variable_and_recursive_home_delete() {
    let collision = shell_params(
        "powershell.exe",
        vec!["-NoProfile", "-Command", "$home = 'C:\\temp\\x'"],
        1000,
    );
    let err = validate_run_shell_params(&collision)
        .expect_err("reserved variable assignment must be refused");
    assert_eq!(
        err.data
            .as_ref()
            .and_then(|d| d.get("code"))
            .and_then(serde_json::Value::as_str),
        Some(error_codes::SAFETY_SHELL_RESERVED_VARIABLE_COLLISION)
    );

    let uncontained = shell_params(
        "powershell.exe",
        vec![
            "-NoProfile",
            "-Command",
            "Remove-Item $home -Recurse -Force",
        ],
        1000,
    );
    let err =
        validate_run_shell_params(&uncontained).expect_err("recursive home delete must be refused");
    let data = err.data.as_ref().expect("structured error data");
    assert_eq!(
        data.get("code").and_then(serde_json::Value::as_str),
        Some(error_codes::SAFETY_SHELL_RECURSIVE_DELETE_UNCONTAINED)
    );
    // The refusal must surface the resolved absolute target, not just the ref.
    #[cfg(windows)]
    assert!(
        data.get("resolved_target")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|target| !target.is_empty()),
        "refusal must surface the resolved home path"
    );
}

#[test]
fn validate_run_shell_params_allows_workspace_recursive_delete() {
    let ok = shell_params(
        "powershell.exe",
        vec![
            "-NoProfile",
            "-Command",
            "Remove-Item C:\\code\\Synapse\\target\\fsv -Recurse -Force",
        ],
        1000,
    );
    validate_run_shell_params(&ok)
        .expect("recursive delete of an explicit workspace path must be allowed");
}

#[cfg(windows)]
#[test]
fn resolve_program_on_path_finds_and_misses() {
    let system32 = format!(
        "{}\\System32",
        std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".to_owned())
    );
    let pathext = WINDOWS_DEFAULT_PATHEXT;
    assert!(
        resolve_program_on_path("findstr", &system32, pathext).is_some(),
        "findstr.exe must resolve under System32"
    );
    assert!(
        resolve_program_on_path("synapse_definitely_not_a_real_tool_xyz", &system32, pathext)
            .is_none(),
        "a nonexistent tool must resolve to None, not a false positive"
    );
}

fn shell_config_for(params: &ActRunShellParams) -> M4ServiceConfig {
    match M4ServiceConfig::from_cli_parts(
        vec![format!("^{}$", regex::escape(&shell_command_line(params)))],
        Vec::new(),
        DEFAULT_RUN_SHELL_INLINE_AWAIT_LIMIT_MS,
    ) {
        Ok(config) => config,
        Err(error) => panic!("synthetic shell allowlist should compile: {error:#}"),
    }
}

fn shell_params(command: &str, args: Vec<&str>, timeout_ms: u64) -> ActRunShellParams {
    ActRunShellParams {
        command: command.to_owned(),
        args: args.into_iter().map(str::to_owned).collect(),
        working_dir: None,
        env: BTreeMap::new(),
        timeout_ms,
        execution_mode: ActRunShellExecutionMode::Auto,
        durable_timeout_ms: None,
        idempotency_key: None,
    }
}

fn temp_shell_job_paths(temp: &tempfile::TempDir) -> ShellJobPaths {
    ShellJobPaths {
        job_dir: temp.path().to_path_buf(),
        stdout_path: temp.path().join("stdout.log"),
        stderr_path: temp.path().join("stderr.log"),
        status_path: temp.path().join("status.json"),
        request_path: temp.path().join("request.json"),
        remote_cleanup_path: temp.path().join("remote-cleanup.json"),
    }
}

fn issue1277_ssh_status(job_id: &str, status: &str, paths: &ShellJobPaths) -> ActRunShellJobStatus {
    let params = ActRunShellStartParams {
        command: "ssh.exe".to_owned(),
        args: vec![
            "aiwonder".to_owned(),
            "bash -lc 'exec -a issue1277 sleep 600'".to_owned(),
        ],
        working_dir: None,
        env: BTreeMap::new(),
        timeout_ms: None,
        job_id: Some(job_id.to_owned()),
    };
    let authorization = RunShellAuthorization {
        command_line: "ssh.exe aiwonder \"bash -lc 'exec -a issue1277 sleep 600'\"".to_owned(),
        matched_pattern: "^ssh".to_owned(),
    };
    shell_job_status_record(
        job_id,
        status,
        &params,
        paths,
        "request-sha",
        &authorization,
        "2026-06-20T00:00:00Z".to_owned(),
        Some(1234),
        None,
    )
}

fn local_model_spawn_params(prompt: Option<&str>) -> ActSpawnAgentParams {
    spawn_params(ActSpawnAgentCli::LocalModel, prompt)
}

fn spawn_params(cli: ActSpawnAgentCli, prompt: Option<&str>) -> ActSpawnAgentParams {
    ActSpawnAgentParams {
        cli: Some(cli),
        kind: Some(cli),
        model: None,
        model_ref: cli.is_local_model().then(|| "qwen8v2-tool-live".to_owned()),
        prompt: prompt.map(str::to_owned),
        target: None,
        working_dir: Some(r"C:\code\Synapse".to_owned()),
        mcp_url: default_agent_spawn_mcp_url(),
        wait_timeout_ms: default_agent_spawn_wait_timeout_ms(),
        hold_open_ms: default_agent_spawn_hold_open_ms(),
        require_approval_gate: default_require_approval_gate(),
        template_id: None,
        template_version: None,
        template_config_hash: None,
    }
}

#[test]
fn local_model_spawn_empty_prompt_errors_before_launch() {
    for prompt in [None, Some(""), Some("   \n\t   ")] {
        let params = local_model_spawn_params(prompt);
        let error = validate_agent_spawn_params(&params)
            .expect_err("blank local-model prompts must fail before launch");
        assert_eq!(
            error.data.as_ref().and_then(|data| data.get("code")),
            Some(&json!(error_codes::TOOL_PARAMS_INVALID))
        );
        assert!(
            error
                .message
                .contains("local_model prompt must not be empty"),
            "{}",
            error.message
        );
    }

    validate_agent_spawn_params(&local_model_spawn_params(Some("call health once")))
        .expect("nonblank local-model prompt remains valid");
}

#[test]
fn direct_primary_spawn_empty_prompt_errors_before_launch() {
    for cli in [ActSpawnAgentCli::Codex, ActSpawnAgentCli::Claude] {
        for prompt in [None, Some(""), Some("  \r\n\t  ")] {
            let params = spawn_params(cli, prompt);
            let error = validate_agent_spawn_params(&params)
                .expect_err("blank direct primary-agent prompts must fail before launch");
            assert_eq!(
                error.data.as_ref().and_then(|data| data.get("code")),
                Some(&json!(error_codes::TOOL_PARAMS_INVALID))
            );
            assert!(
                error
                    .message
                    .contains("direct spawn prompt must not be empty"),
                "{}",
                error.message
            );
        }

        validate_agent_spawn_params(&spawn_params(cli, Some("call health once")))
            .expect("nonblank direct primary-agent prompt remains valid");
    }
}

#[test]
fn template_rendered_primary_spawn_prompt_remains_template_governed() {
    let mut params = spawn_params(ActSpawnAgentCli::Codex, Some("template task"));
    params.template_id = Some("issue1245-template".to_owned());
    params.template_version = Some(1);
    params.template_config_hash = Some("sha256:test".to_owned());

    validate_agent_spawn_params(&params).expect("template-rendered nonblank prompt remains valid");
}

fn foreground_for_launch_selection(
    hwnd: i64,
    pid: u32,
    process_name: &str,
    window_title: &str,
) -> ForegroundContext {
    ForegroundContext {
        hwnd,
        pid,
        process_name: process_name.to_owned(),
        process_path: format!(r"C:\Synthetic\{process_name}"),
        window_title: window_title.to_owned(),
        window_bounds: synapse_core::Rect {
            x: 0,
            y: 0,
            w: 640,
            h: 480,
        },
        monitor_index: 0,
        dpi_scale: 1.0,
        profile_id: None,
        steam_appid: None,
        is_fullscreen: false,
        is_dwm_composed: true,
    }
}

#[cfg(windows)]
#[test]
fn child_environment_derives_windows_profile_paths_from_slim_daemon_env() {
    let mut env = BTreeMap::new();
    env.insert(
        "USERPROFILE".to_owned(),
        ("USERPROFILE".to_owned(), r"C:\Users\hotra".to_owned()),
    );
    env.insert(
        "SYSTEMDRIVE".to_owned(),
        ("SystemDrive".to_owned(), "D:".to_owned()),
    );

    add_windows_profile_environment(&mut env);

    assert_eq!(
        env.get("APPDATA").map(|(_key, value)| value.as_str()),
        Some(r"C:\Users\hotra\AppData\Roaming")
    );
    assert_eq!(
        env.get("LOCALAPPDATA").map(|(_key, value)| value.as_str()),
        Some(r"C:\Users\hotra\AppData\Local")
    );
    assert_eq!(
        env.get("PROGRAMDATA").map(|(_key, value)| value.as_str()),
        Some(r"D:\ProgramData")
    );
    println!(
        "readback=child_env edge=slim_daemon after_appdata={} after_localappdata={} after_programdata={}",
        env["APPDATA"].1, env["LOCALAPPDATA"].1, env["PROGRAMDATA"].1
    );
}

#[cfg(windows)]
#[test]
fn shell_spawn_command_prefers_git_ssh_for_bare_windows_ssh_family() {
    let dir =
        tempfile::TempDir::new().unwrap_or_else(|error| panic!("create temp git ssh dir: {error}"));
    for leaf in ["ssh.exe", "scp.exe", "sftp.exe"] {
        std::fs::write(dir.path().join(leaf), b"synthetic git ssh binary")
            .unwrap_or_else(|error| panic!("write {leaf}: {error}"));
    }
    let dirs = vec![dir.path().to_path_buf()];

    let ssh = resolve_windows_ssh_family_spawn_command_with_dirs("ssh", &dirs)
        .unwrap_or_else(|| panic!("bare ssh should resolve"));
    let scp = resolve_windows_ssh_family_spawn_command_with_dirs("scp.exe", &dirs)
        .unwrap_or_else(|| panic!("bare scp.exe should resolve"));
    let sftp = resolve_windows_ssh_family_spawn_command_with_dirs("sftp", &dirs)
        .unwrap_or_else(|| panic!("bare sftp should resolve"));

    println!(
        "readback=act_run_shell_spawn_resolution edge=bare_ssh before=ssh/scp/sftp after_ssh={ssh} after_scp={scp} after_sftp={sftp}"
    );
    assert_eq!(ssh, dir.path().join("ssh.exe").to_string_lossy());
    assert_eq!(scp, dir.path().join("scp.exe").to_string_lossy());
    assert_eq!(sftp, dir.path().join("sftp.exe").to_string_lossy());

    assert_eq!(
        resolve_windows_ssh_family_spawn_command_with_dirs(
            r"C:\Windows\System32\OpenSSH\ssh.exe",
            &dirs
        ),
        None
    );
    assert_eq!(
        resolve_windows_ssh_family_spawn_command_with_dirs(r".\ssh.exe", &dirs),
        None
    );
    assert_eq!(
        resolve_windows_ssh_family_spawn_command_with_dirs("powershell.exe", &dirs),
        None
    );
}

#[cfg(windows)]
#[test]
fn shell_spawn_command_does_not_use_incomplete_git_ssh_directory() {
    let dir = tempfile::TempDir::new()
        .unwrap_or_else(|error| panic!("create incomplete git ssh dir: {error}"));
    std::fs::write(dir.path().join("ssh.exe"), b"synthetic git ssh binary")
        .unwrap_or_else(|error| panic!("write ssh.exe: {error}"));
    let dirs = vec![dir.path().to_path_buf()];

    let resolved = resolve_windows_ssh_family_spawn_command_with_dirs("ssh", &dirs);

    println!(
        "readback=act_run_shell_spawn_resolution edge=incomplete_git_dir before=ssh_only after={resolved:?}"
    );
    assert_eq!(resolved, None);
}

#[cfg(windows)]
#[test]
fn child_path_prefers_git_ssh_before_windows_openssh() {
    let git_dir = r"C:\Program Files\Git\usr\bin";
    let openssh_dir = r"C:\Windows\System32\OpenSSH";
    let before = r"C:\Windows\System32;C:\Windows\System32\OpenSSH;C:\Program Files\Git\usr\bin;C:\Tools;C:\Windows\System32\OpenSSH";

    let after =
        reorder_semicolon_path_entry_before_targets(before, git_dir, &[openssh_dir.to_owned()]);
    let parts = after.split(';').collect::<Vec<_>>();
    let git_index = parts
        .iter()
        .position(|part| {
            normalize_semicolon_path_part(part) == normalize_semicolon_path_part(git_dir)
        })
        .unwrap_or_else(|| panic!("git ssh dir should be present"));
    let openssh_index = parts
        .iter()
        .position(|part| {
            normalize_semicolon_path_part(part) == normalize_semicolon_path_part(openssh_dir)
        })
        .unwrap_or_else(|| panic!("windows openssh dir should be present"));
    let git_count = parts
        .iter()
        .filter(|part| {
            normalize_semicolon_path_part(part) == normalize_semicolon_path_part(git_dir)
        })
        .count();

    println!("readback=child_env_path edge=git_before_openssh before={before} after={after}");
    assert!(git_index < openssh_index);
    assert_eq!(git_count, 1);
}

#[cfg(windows)]
#[test]
fn child_environment_preserves_explicit_appdata_from_daemon_env() {
    let mut env = BTreeMap::new();
    env.insert(
        "USERPROFILE".to_owned(),
        ("USERPROFILE".to_owned(), r"C:\Users\hotra".to_owned()),
    );
    env.insert(
        "APPDATA".to_owned(),
        ("APPDATA".to_owned(), r"E:\Roaming".to_owned()),
    );

    add_windows_profile_environment(&mut env);

    assert_eq!(
        env.get("APPDATA").map(|(_key, value)| value.as_str()),
        Some(r"E:\Roaming")
    );
    println!(
        "readback=child_env edge=explicit_appdata after_appdata={}",
        env["APPDATA"].1
    );
}

fn launch_config_for(params: &ActLaunchParams) -> M4ServiceConfig {
    let command_line = launch_command_line(params)
        .unwrap_or_else(|error| panic!("synthetic launch command line should build: {error}"));
    match M4ServiceConfig::from_cli_parts(
        Vec::new(),
        vec![format!("^{}$", regex::escape(&command_line))],
        DEFAULT_RUN_SHELL_INLINE_AWAIT_LIMIT_MS,
    ) {
        Ok(config) => config,
        Err(error) => panic!("synthetic launch allowlist should compile: {error:#}"),
    }
}

fn launch_params(target: &str, args: Vec<&str>, timeout_ms: u64) -> ActLaunchParams {
    ActLaunchParams {
        target: target.to_owned(),
        args: args.into_iter().map(str::to_owned).collect(),
        working_dir: None,
        env: BTreeMap::new(),
        wait_for_window_title_regex: None,
        timeout_ms,
        idempotency_key: None,
        cdp_debug: None,
        force_renderer_accessibility: None,
        windows_console_window_state: None,
        desktop: None,
    }
}

#[test]
fn launch_desktop_option_rejects_invalid_shapes() {
    let cases = [
        ("", "desktop_empty_or_padded"),
        (" agent:session", "desktop_empty_or_padded"),
        ("agent:", "desktop_agent_scope_empty"),
        ("existing:", "desktop_existing_name_invalid"),
        ("existing:bad\\name", "desktop_existing_name_invalid"),
        ("default", "desktop_scope_unsupported"),
    ];

    for (desktop, reason) in cases {
        let mut params = launch_params("notepad.exe", Vec::new(), 10_000);
        params.desktop = Some(desktop.to_owned());
        let error =
            validate_launch_params(&params).expect_err("invalid desktop shape should fail closed");
        println!(
            "readback=act_launch_desktop_validation edge={reason} before={desktop:?} after={:?}",
            error.data
        );
        assert_eq!(
            error
                .data
                .as_ref()
                .and_then(|data| data.get("reason"))
                .and_then(|reason| reason.as_str()),
            Some(reason)
        );
    }
}

#[test]
fn launch_desktop_option_accepts_window_wait_for_hidden_desktop_readback() {
    let mut params = launch_params(r"C:\Windows\notepad.exe", Vec::new(), 10_000);
    params.desktop = Some("agent:session".to_owned());
    params.wait_for_window_title_regex = Some("^owned-hidden-notepad$".to_owned());

    validate_launch_params(&params)
        .expect("desktop launch waits are supported through hidden-desktop enumeration");
}

#[test]
fn launch_desktop_agent_scope_is_session_bound() {
    let error = prepare_launch_desktop(Some("agent:other-session"), Some("current-session"))
        .expect_err("agent desktop scope must match current session");

    println!(
        "readback=act_launch_desktop_scope edge=session_mismatch before=request:agent:other-session,current:current-session after={:?}",
        error.data
    );
    assert_eq!(
        error
            .data
            .as_ref()
            .and_then(|data| data.get("reason"))
            .and_then(|reason| reason.as_str()),
        Some("desktop_agent_session_mismatch")
    );
    assert_eq!(
        hidden_desktop_name_for_session("current-session"),
        hidden_desktop_name_for_session("current-session")
    );
    assert_ne!(
        hidden_desktop_name_for_session("current-session"),
        hidden_desktop_name_for_session("other-session")
    );
    assert!(hidden_desktop_name_for_session("current-session").len() <= 128);
}

#[test]
fn chromium_cdp_launch_injects_ephemeral_port_and_dedicated_profile() {
    let params = launch_params("chrome.exe", vec!["https://example.com"], 10_000);
    let launch = chromium_cdp_launch(&params).expect("chrome.exe should get CDP injection");
    println!(
        "readback=cdp_launch edge=chrome before=args:{:?} after=injected:{:?} udd:{:?}",
        params.args, launch.injected_args, launch.user_data_dir
    );
    assert!(
        launch
            .injected_args
            .iter()
            .any(|arg| arg == "--remote-debugging-port=0")
    );
    assert!(
        launch
            .injected_args
            .iter()
            .any(|arg| arg.starts_with("--user-data-dir="))
    );
    assert!(
        launch
            .injected_args
            .iter()
            .any(|arg| arg == "--silent-debugger-extension-api")
    );
    assert!(
        launch
            .injected_args
            .iter()
            .any(|arg| arg == "--disable-extensions")
    );
    // The dedicated profile must be non-default (Chrome 136+ requirement).
    assert!(
        launch
            .user_data_dir
            .to_string_lossy()
            .contains("synapse-cdp-profiles")
    );

    let spawn_params = params_with_chromium_launch_args(&params, Some(&launch), None);
    // Injected flags precede the caller's URL so the positional arg parses.
    assert_eq!(
        spawn_params.args.first().map(String::as_str),
        Some("--remote-debugging-port=0")
    );
    assert_eq!(
        spawn_params.args.last().map(String::as_str),
        Some("https://example.com")
    );
}

#[test]
fn chromium_cdp_launch_respects_opt_out_and_non_chromium() {
    let mut opted_out = launch_params("chrome.exe", vec![], 10_000);
    opted_out.cdp_debug = Some(false);
    println!("readback=cdp_launch edge=opt_out before=cdp_debug:Some(false)");
    assert!(chromium_cdp_launch(&opted_out).is_none());

    let notepad = launch_params("notepad.exe", vec![], 10_000);
    println!("readback=cdp_launch edge=non_chromium before=target:notepad.exe");
    assert!(chromium_cdp_launch(&notepad).is_none());
}

#[test]
fn chromium_cdp_launch_defers_to_popup_safe_caller_supplied_flags() {
    let with_port = launch_params("msedge.exe", vec!["--remote-debugging-port=9222"], 10_000);
    println!(
        "readback=cdp_launch edge=caller_port before=args:{:?}",
        with_port.args
    );
    assert!(chromium_cdp_launch(&with_port).is_none());
    let error = validate_launch_params(&with_port).expect_err("unsafe debug launch must fail");
    assert_eq!(
        extract_error_code(&error),
        error_codes::A11Y_CDP_DEBUGGER_WARNING_UNSUPPRESSED
    );
    assert!(
        error
            .message
            .contains("refused a Chromium remote-debugging launch")
    );

    let with_profile = launch_params("chrome.exe", vec!["--user-data-dir=C:\\my"], 10_000);
    assert!(chromium_cdp_launch(&with_profile).is_none());

    let safe_profile = cdp_automation_profile_dir();
    let safe_profile_arg = format!("--user-data-dir={}", safe_profile.display());
    let safe_remote_debug = launch_params(
        "chrome.exe",
        vec![
            "--remote-debugging-port=0",
            safe_profile_arg.as_str(),
            "--silent-debugger-extension-api",
            "--disable-extensions",
            "about:blank",
        ],
        10_000,
    );
    println!(
        "readback=cdp_launch edge=caller_popup_safe before=args:{:?}",
        safe_remote_debug.args
    );
    validate_launch_params(&safe_remote_debug).expect("popup-safe caller CDP launch");

    let banner_profile = cdp_automation_profile_dir();
    let banner_profile_arg = format!("--user-data-dir={}", banner_profile.display());
    let banner_remote_debug = launch_params(
        "chrome.exe",
        vec![
            "--remote-debugging-pipe",
            banner_profile_arg.as_str(),
            "--silent-debugger-extension-api",
            "--disable-extensions",
            "--disable-blink-features=AutomationControlled",
            "about:blank",
        ],
        10_000,
    );
    let error = validate_launch_params(&banner_remote_debug)
        .expect_err("layout-shifting Chrome warning flags must fail closed");
    assert_eq!(
        extract_error_code(&error),
        error_codes::A11Y_CDP_DEBUGGER_WARNING_UNSUPPRESSED
    );
    assert!(error.message.contains("remote-debugging launch"));
    let error_text = format!("{error:?}");
    assert!(error_text.contains("has_layout_shifting_infobar_flags"));
    assert!(error_text.contains("AutomationControlled"));
}

#[test]
fn chromium_renderer_accessibility_is_opt_in_and_chromium_only() {
    let mut params = launch_params("chrome.exe", vec!["https://example.com"], 10_000);
    println!(
        "readback=renderer_a11y edge=default before=force_renderer_accessibility:{:?}",
        params.force_renderer_accessibility
    );
    assert!(chromium_renderer_accessibility_arg(&params).is_none());

    params.force_renderer_accessibility = Some(true);
    let arg = chromium_renderer_accessibility_arg(&params);
    println!(
        "readback=renderer_a11y edge=opt_in before=args:{:?} after=arg:{arg:?}",
        params.args
    );
    assert_eq!(arg.as_deref(), Some("--force-renderer-accessibility"));

    let launch = chromium_cdp_launch(&params).expect("chrome should still get CDP launch");
    let spawn_params = params_with_chromium_launch_args(&params, Some(&launch), arg);
    assert!(
        spawn_params
            .args
            .iter()
            .any(|arg| arg == "--force-renderer-accessibility")
    );
    assert_eq!(
        spawn_params.args.last().map(String::as_str),
        Some("https://example.com")
    );

    let mut notepad = launch_params("notepad.exe", vec![], 10_000);
    notepad.force_renderer_accessibility = Some(true);
    assert!(chromium_renderer_accessibility_arg(&notepad).is_none());
}

#[test]
fn chromium_renderer_accessibility_respects_caller_flag_and_truthy_env_values() {
    let mut caller = launch_params(
        "msedge.exe",
        vec!["--force-renderer-accessibility", "https://example.com"],
        10_000,
    );
    caller.force_renderer_accessibility = Some(true);
    assert!(
        chromium_renderer_accessibility_arg(&caller).is_none(),
        "caller-supplied flag must not be duplicated"
    );

    caller.args[0] = "--force-renderer-accessibility=complete".to_owned();
    assert!(
        chromium_renderer_accessibility_arg(&caller).is_none(),
        "caller-supplied valued flag must not be duplicated"
    );

    for value in ["1", "true", "yes", "on", " TRUE "] {
        assert!(truthy_value(value), "{value:?} should enable env opt-in");
    }
    for value in ["", "0", "false", "off", "no", "maybe"] {
        assert!(
            !truthy_value(value),
            "{value:?} should not enable env opt-in"
        );
    }
}

#[test]
fn read_devtools_active_port_parses_first_line() {
    let dir = std::env::temp_dir().join(format!(
        "synapse-cdp-test-{}-{}",
        std::process::id(),
        line!()
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let port_file = dir.join("DevToolsActivePort");
    std::fs::write(&port_file, "51234\n/devtools/browser/abc-123\n").expect("write port file");
    let port = read_devtools_active_port(&port_file);
    println!("readback=devtools_active_port before=file:{port_file:?} after=port:{port:?}");
    assert_eq!(port, Some(51234));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn launch_requested_url_detects_browser_page_args() {
    let args = vec![
        "--new-window".to_owned(),
        "http://localhost:5173/polis".to_owned(),
    ];
    let url = launch_requested_url(&args);
    println!("readback=act_launch_url edge=wsl_localhost_arg before=args:{args:?} after={url:?}");
    assert_eq!(url.as_deref(), Some("http://localhost:5173/polis"));

    let app_args = vec!["--app=https://example.test/app".to_owned()];
    assert_eq!(
        launch_requested_url(&app_args).as_deref(),
        Some("https://example.test/app")
    );

    let non_url_args = vec!["--new-window".to_owned(), "not-a-url".to_owned()];
    assert!(launch_requested_url(&non_url_args).is_none());
}

#[tokio::test]
async fn launch_url_verification_skips_when_synapse_did_not_open_cdp() {
    let mut opted_out = launch_params("chrome.exe", vec!["http://localhost:5173"], 10);
    opted_out.cdp_debug = Some(false);
    let result = verify_launched_chromium_url(&opted_out, None, &LaunchedCdp::default(), 10).await;
    println!("readback=act_launch_url edge=cdp_opt_out before=cdp_launch:None after={result:?}");
    assert!(matches!(result, Ok(None)));

    let non_chromium = launch_params("notepad.exe", vec!["http://localhost:5173"], 10);
    let result =
        verify_launched_chromium_url(&non_chromium, None, &LaunchedCdp::default(), 10).await;
    println!("readback=act_launch_url edge=non_chromium before=cdp_launch:None after={result:?}");
    assert!(matches!(result, Ok(None)));
}

#[test]
fn launch_url_matching_normalizes_root_trailing_slash() {
    assert!(url_matches(
        "http://localhost:5173",
        "http://localhost:5173/"
    ));
    assert!(url_matches(
        "https://example.test/path?q=1",
        "https://example.test/path?q=1"
    ));
    assert!(!url_matches(
        "http://localhost:5173/expected",
        "http://localhost:5173/other"
    ));
}

fn combo_press_step(at_ms: u32, key: &str) -> ActComboStep {
    ActComboStep {
        at_ms,
        action: ActComboAction::ActPress,
        params: json!({
            "keys": [key],
            "hold_ms": 1,
            "backend": "software",
        }),
        backend: None,
    }
}

fn combo_params(steps: Vec<ActComboStep>) -> ActComboParams {
    ActComboParams {
        steps,
        backend: Backend::Software,
        idempotency_key: None,
    }
}

fn assert_tool_params_invalid(error: &ErrorData) {
    assert_eq!(
        error
            .data
            .as_ref()
            .and_then(|data| data.get("code"))
            .and_then(|code| code.as_str()),
        Some(error_codes::TOOL_PARAMS_INVALID)
    );
}

#[test]
fn combo_rejects_empty_steps() {
    let error = match validate_combo_params(&combo_params(Vec::new())) {
        Ok(()) => panic!("empty combo should reject"),
        Err(error) => error,
    };

    assert_tool_params_invalid(&error);
    assert!(
        error
            .message
            .contains("steps must contain at least one step")
    );
}

#[test]
fn combo_rejects_more_than_256_steps() {
    let steps = (0..=MAX_COMBO_STEPS)
        .map(|index| combo_press_step(u32::try_from(index).unwrap_or(u32::MAX), "f13"))
        .collect::<Vec<_>>();
    let error = match validate_combo_params(&combo_params(steps)) {
        Ok(()) => panic!("257-step combo should reject"),
        Err(error) => error,
    };

    assert_tool_params_invalid(&error);
    assert!(error.message.contains("exceeds max 256"));
}

#[test]
fn combo_rejects_non_monotonic_steps() {
    let error = match validate_combo_params(&combo_params(vec![
        combo_press_step(10, "f14"),
        combo_press_step(9, "f15"),
    ])) {
        Ok(()) => panic!("non-monotonic combo should reject"),
        Err(error) => error,
    };

    assert_tool_params_invalid(&error);
    assert!(error.message.contains("at_ms must be monotonic"));
}

#[test]
fn combo_rejects_motion_action_with_act_stroke_pointer() {
    let params = combo_params(vec![ActComboStep {
        at_ms: 0,
        action: ActComboAction::Retired("act_drag".to_owned()),
        params: json!({"path": [{"x": 0, "y": 0}, {"x": 10, "y": 0}]}),
        backend: None,
    }]);
    let error = match combo_steps_from_params(&params) {
        Ok(steps) => panic!("motion combo action should reject, got {steps:?}"),
        Err(error) => error,
    };

    assert_tool_params_invalid(&error);
    assert!(error.message.contains("act_drag"));
    assert!(error.message.contains("not combo-lowerable"));
    assert!(error.message.contains("Use act_stroke"));
}

#[test]
fn combo_rejects_nested_press_backend_mismatch() {
    let params = ActComboParams {
        steps: vec![ActComboStep {
            at_ms: 0,
            action: ActComboAction::ActPress,
            params: json!({
                "keys": ["f17"],
                "hold_ms": 5,
                "backend": "hardware"
            }),
            backend: None,
        }],
        backend: Backend::Software,
        idempotency_key: None,
    };

    let error = match combo_steps_from_params(&params) {
        Ok(steps) => panic!("mismatched backend should reject, got {steps:?}"),
        Err(error) => error,
    };

    assert_eq!(
        error
            .data
            .as_ref()
            .and_then(|data| data.get("code"))
            .and_then(|code| code.as_str()),
        Some(error_codes::TOOL_PARAMS_INVALID)
    );
    assert!(
        error
            .message
            .contains("act_press params.backend differs from top-level backend")
    );
}

#[test]
fn combo_allows_nested_press_auto_backend_to_use_top_level_backend() {
    let params = ActComboParams {
        steps: vec![ActComboStep {
            at_ms: 0,
            action: ActComboAction::ActPress,
            params: json!({
                "keys": ["f18"],
                "hold_ms": 5,
                "backend": "auto"
            }),
            backend: None,
        }],
        backend: Backend::Software,
        idempotency_key: None,
    };

    let steps = match combo_steps_from_params(&params) {
        Ok(steps) => steps,
        Err(error) => panic!("auto backend should lower: {error}"),
    };

    assert_eq!(steps.len(), 1);
    assert_eq!(steps[0].at_ms, 0);
}

#[test]
fn shell_command_line_quotes_empty_and_space_args() {
    let params = shell_params("cmd.exe", vec!["/c", "echo hello", ""], 30_000);

    assert_eq!(
        shell_command_line(&params),
        "cmd.exe /c \"echo hello\" \"\""
    );
}

// Regression for #1204: the witnessed SendKeys command used shell-based
// global input to select a background Chrome tab, which landed on the human
// foreground window. It must be denied before the allowlist check can permit
// the exact command line.
#[test]
fn run_shell_rejects_global_sendkeys_input() {
    let params = shell_params(
        "powershell",
        vec![
            "-NoProfile",
            "-Command",
            "Add-Type -AssemblyName System.Windows.Forms; [System.Windows.Forms.SendKeys]::SendWait('^9'); Start-Sleep -Milliseconds 750",
        ],
        5_000,
    );
    let error = authorize_run_shell(&shell_config_for(&params), &params)
        .expect_err("a SendKeys global-input command must be denied");

    assert_eq!(
        error.data.as_ref().and_then(|data| data.get("code")),
        Some(&json!(error_codes::SAFETY_SHELL_GLOBAL_INPUT_DENIED))
    );
    assert_eq!(
        error
            .data
            .as_ref()
            .and_then(|data| data.get("matched_marker"))
            .and_then(|marker| marker.as_str()),
        Some("sendkeys")
    );
}

#[test]
fn run_shell_rejects_each_global_input_marker() {
    for snippet in [
        "[System.Windows.Forms.SendKeys]::Send('a')",
        "$wsh.SendKeys('{ENTER}')",
        "[Win32]::SendInput($n, $inputs, $size)",
        "keybd_event(0x41, 0, 0, 0)",
        "mouse_event(2, 0, 0, 0, 0)",
        "[Win32]::SetCursorPos(10, 10)",
        "[Win32]::SetForegroundWindow($h)",
        "[Win32]::BringWindowToTop($h)",
        "AutoHotkey.exe send.ahk",
    ] {
        let params = shell_params("powershell", vec!["-Command", snippet], 5_000);
        let error = authorize_run_shell(&shell_config_for(&params), &params)
            .expect_err("global-input snippet must be denied");

        assert_eq!(
            error.data.as_ref().and_then(|data| data.get("code")),
            Some(&json!(error_codes::SAFETY_SHELL_GLOBAL_INPUT_DENIED)),
            "global-input snippet must be denied: {snippet}"
        );
    }
}

#[test]
fn run_shell_allows_ordinary_commands_without_global_input() {
    for params in [
        shell_params("cmd.exe", vec!["/c", "echo hello"], 5_000),
        shell_params("powershell", vec!["-Command", "Get-Process chrome"], 5_000),
        shell_params("git", vec!["status", "--short"], 5_000),
        shell_params("cargo", vec!["check", "-p", "synapse-core"], 5_000),
    ] {
        assert!(
            detect_shell_global_input(&shell_command_line(&params)).is_none(),
            "benign command must not be flagged as global input: {}",
            shell_command_line(&params)
        );
        authorize_run_shell(&shell_config_for(&params), &params)
            .unwrap_or_else(|error| panic!("benign command must authorize: {error}"));
    }
}

#[test]
fn shell_command_metadata_redacts_large_and_secret_args() {
    let large_body = format!(
        "$body = @'\n{}\n'@; $body | gh issue comment 1 --body-file -",
        "SYN877-LARGE-BODY-DO-NOT-ECHO ".repeat(12)
    );
    let secret_arg = "synapse_token_0123456789abcdef0123456789abcdef";
    let args = vec![
        "-NoProfile".to_owned(),
        "-Command".to_owned(),
        large_body.clone(),
        secret_arg.to_owned(),
    ];

    let metadata = shell_command_metadata("powershell.exe", &args);

    println!(
        "readback=act_run_shell_metadata edge=large_secret before=large_bytes:{} token_bytes:{} after={metadata:?}",
        large_body.len(),
        secret_arg.len()
    );
    assert!(metadata.args_redacted);
    assert!(metadata.command_line_redacted);
    assert_eq!(metadata.args_original_count, 4);
    assert!(metadata.args_sha256.len() == 64);
    assert!(metadata.command_line_sha256.len() == 64);
    assert!(
        !metadata
            .args
            .iter()
            .any(|arg| arg.contains("SYN877-LARGE-BODY"))
    );
    assert!(!metadata.args.iter().any(|arg| arg.contains(secret_arg)));
    assert!(!metadata.command_line.contains("SYN877-LARGE-BODY"));
    assert!(!metadata.command_line.contains(secret_arg));
}

#[test]
fn shell_job_status_and_request_store_safe_command_metadata() {
    let temp = tempfile::TempDir::new()
        .unwrap_or_else(|error| panic!("create temp shell status dir: {error}"));
    let paths = ShellJobPaths {
        job_dir: temp.path().to_path_buf(),
        stdout_path: temp.path().join("stdout.log"),
        stderr_path: temp.path().join("stderr.log"),
        status_path: temp.path().join("status.json"),
        request_path: temp.path().join("request.json"),
        remote_cleanup_path: temp.path().join("remote-cleanup.json"),
    };
    let raw_body = format!(
        "Write-Output '{}'",
        "SYN877-REQUEST-BODY-DO-NOT-PERSIST ".repeat(10)
    );
    let params = ActRunShellStartParams {
        command: "powershell.exe".to_owned(),
        args: vec![
            "-NoProfile".to_owned(),
            "-Command".to_owned(),
            raw_body.clone(),
        ],
        working_dir: None,
        env: BTreeMap::new(),
        timeout_ms: None,
        job_id: Some("issue877-safe-metadata".to_owned()),
    };
    let authorization = RunShellAuthorization {
        command_line: shell_command_line_from_parts(&params.command, &params.args),
        matched_pattern: "__any_permitted__".to_owned(),
    };
    let request_sha = run_shell_start_request_sha256(&params)
        .unwrap_or_else(|error| panic!("start request should hash: {error}"));

    write_shell_job_request(&paths, &params, &request_sha, None)
        .unwrap_or_else(|error| panic!("request should write: {error}"));
    let status = shell_job_status_record(
        "issue877-safe-metadata",
        "running",
        &params,
        &paths,
        &request_sha,
        &authorization,
        "2026-06-11T00:00:00Z".to_owned(),
        Some(1234),
        None,
    );
    write_shell_job_status(&paths.status_path, &status)
        .unwrap_or_else(|error| panic!("status should write: {error}"));
    let request_json = std::fs::read_to_string(&paths.request_path)
        .unwrap_or_else(|error| panic!("request should read: {error}"));
    let status_json = std::fs::read_to_string(&paths.status_path)
        .unwrap_or_else(|error| panic!("status should read: {error}"));
    let read_status = read_shell_job_status(&paths.status_path, "issue877-safe-metadata")
        .unwrap_or_else(|error| panic!("status should decode: {error}"));

    println!(
        "readback=act_run_shell_metadata edge=status_request before=raw_bytes:{} after=request:{} status:{}",
        raw_body.len(),
        request_json,
        status_json
    );
    assert!(!request_json.contains("SYN877-REQUEST-BODY-DO-NOT-PERSIST"));
    assert!(!status_json.contains("SYN877-REQUEST-BODY-DO-NOT-PERSIST"));
    assert!(
        !read_status
            .command_line
            .contains("SYN877-REQUEST-BODY-DO-NOT-PERSIST")
    );
    assert!(read_status.args_redacted);
    assert!(read_status.command_line_redacted);
    assert_eq!(
        read_status.command_metadata_policy,
        SHELL_COMMAND_METADATA_POLICY
    );
    assert_eq!(read_status.args_original_count, 3);
    assert_eq!(read_status.request_sha256, request_sha);
    assert!(read_status.args_sha256.len() == 64);
    assert!(read_status.command_line_sha256.len() == 64);
}

#[test]
fn shell_job_status_rewrite_has_no_missing_poll_window() {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
        thread,
    };

    let temp = tempfile::TempDir::new()
        .unwrap_or_else(|error| panic!("create temp shell status dir: {error}"));
    let paths = ShellJobPaths {
        job_dir: temp.path().to_path_buf(),
        stdout_path: temp.path().join("stdout.log"),
        stderr_path: temp.path().join("stderr.log"),
        status_path: temp.path().join("status.json"),
        request_path: temp.path().join("request.json"),
        remote_cleanup_path: temp.path().join("remote-cleanup.json"),
    };
    let params = ActRunShellStartParams {
        command: "powershell.exe".to_owned(),
        args: vec![
            "-NoProfile".to_owned(),
            "-Command".to_owned(),
            "Write-Output issue1012-status-race".to_owned(),
        ],
        working_dir: None,
        env: BTreeMap::new(),
        timeout_ms: None,
        job_id: Some("issue1012-status-race".to_owned()),
    };
    let authorization = RunShellAuthorization {
        command_line: shell_command_line_from_parts(&params.command, &params.args),
        matched_pattern: "__any_permitted__".to_owned(),
    };
    let request_sha = run_shell_start_request_sha256(&params)
        .unwrap_or_else(|error| panic!("start request should hash: {error}"));
    let mut status = shell_job_status_record(
        "issue1012-status-race",
        "running",
        &params,
        &paths,
        &request_sha,
        &authorization,
        "2026-06-15T00:00:00Z".to_owned(),
        Some(4242),
        None,
    );
    write_shell_job_status(&paths.status_path, &status)
        .unwrap_or_else(|error| panic!("initial status should write: {error}"));

    let stop = Arc::new(AtomicBool::new(false));
    let read_errors = Arc::new(AtomicUsize::new(0));
    let status_path = paths.status_path.clone();
    let reader_stop = Arc::clone(&stop);
    let reader_errors = Arc::clone(&read_errors);
    let reader = thread::spawn(move || {
        while !reader_stop.load(Ordering::Relaxed) {
            if read_shell_job_status(&status_path, "issue1012-status-race").is_err() {
                reader_errors.fetch_add(1, Ordering::Relaxed);
            }
            thread::yield_now();
        }
    });

    for iteration in 0..1_000 {
        status.duration_ms = Some(iteration);
        status.status = if iteration % 2 == 0 {
            "running".to_owned()
        } else {
            "finalizing".to_owned()
        };
        write_shell_job_status(&paths.status_path, &status)
            .unwrap_or_else(|error| panic!("status rewrite should commit: {error}"));
    }

    stop.store(true, Ordering::Relaxed);
    reader
        .join()
        .unwrap_or_else(|error| panic!("reader thread should join: {error:?}"));
    let final_readback = read_shell_job_status(&paths.status_path, "issue1012-status-race")
        .unwrap_or_else(|error| panic!("final status should read: {error}"));

    println!(
        "readback=act_run_shell_status edge=status_replace_race before=1000_rewrites after=read_errors:{} final_status:{}",
        read_errors.load(Ordering::Relaxed),
        final_readback.status
    );
    assert_eq!(read_errors.load(Ordering::Relaxed), 0);
    assert_eq!(final_readback.job_id, "issue1012-status-race");
}

// #1509/#1568: the status reader tolerates the Windows atomic-replace window
// (destination transiently reports NOT_FOUND while a writer's unique staging
// sibling is renamed in) WITHOUT slowing down the genuinely-missing path.
// Both arms are asserted against real filesystem state so a future change
// that either drops the mid-replace tolerance or blanket-retries every
// NOT_FOUND is caught. The mid-replace arm lands the file via the REAL
// atomic write path (unique temp -> fsync -> rename), so the reader can only
// ever observe a whole file — never the empty/partial slice a truncate-in-
// place writer used to expose (the flaky failure surfaced in #1568).
#[cfg(windows)]
#[test]
fn shell_status_read_notfound_gate_distinguishes_replace_from_missing() {
    let temp =
        tempfile::TempDir::new().unwrap_or_else(|error| panic!("create temp status dir: {error}"));
    let status_path = temp.path().join("status.json");

    // Arm 1 — genuinely missing: neither the target nor any staging sibling
    // exists, so the read must fail immediately rather than burning the
    // 500 ms replace-tolerance window.
    assert!(!status_path.exists());
    assert!(!shell_status_replace_in_flight(&status_path));
    let started = Instant::now();
    let missing = read_shell_status_bytes(&status_path);
    let missing_elapsed = started.elapsed();
    println!(
        "readback=read_shell_status_bytes edge=genuine_missing after=err:{} elapsed_ms:{}",
        missing.is_err(),
        missing_elapsed.as_millis()
    );
    assert!(missing.is_err(), "absent status file must error");
    assert_eq!(
        missing.err().and_then(|error| error.raw_os_error()),
        Some(2),
        "missing file must surface ERROR_FILE_NOT_FOUND"
    );
    assert!(
        missing_elapsed < Duration::from_millis(200),
        "genuine missing read must return promptly, took {missing_elapsed:?}"
    );

    // Arm 2 — mid-replace window: target absent but a writer's unique staging
    // sibling is present, so the reader retries. A concurrent thread
    // atomically lands the real file (unique temp -> fsync -> rename, exactly
    // as `write_shell_job_status` does); the read must then succeed with the
    // delivered WHOLE-file bytes, never an empty/partial slice. No timing
    // lower bound is asserted — correctness must not depend on the reader
    // happening to catch the window mid-flight.
    let delivered = br#"{"delivered":true}"#.to_vec();
    let seed_staging = shell_status_temp_path(&status_path);
    std::fs::write(&seed_staging, b"pending-replace")
        .unwrap_or_else(|error| panic!("seed staging sibling: {error}"));
    assert!(
        shell_status_replace_in_flight(&status_path),
        "seeded staging sibling must register as an in-flight replace"
    );
    let writer_status_path = status_path.clone();
    let writer_bytes = delivered.clone();
    let seed_for_writer = seed_staging.clone();
    let writer = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(60));
        let landing = shell_status_temp_path(&writer_status_path);
        write_shell_job_status_staging(&landing, &writer_bytes)
            .unwrap_or_else(|error| panic!("stage replacement status: {error}"));
        commit_shell_job_status_file(&landing, &writer_status_path, "issue1568-mid-replace")
            .unwrap_or_else(|error| panic!("commit replacement status: {error}"));
        let _ = std::fs::remove_file(&seed_for_writer);
    });
    let recovered = read_shell_status_bytes(&status_path);
    writer
        .join()
        .unwrap_or_else(|error| panic!("writer thread should join: {error:?}"));
    println!(
        "readback=read_shell_status_bytes edge=mid_replace after=ok:{}",
        recovered.is_ok()
    );
    assert_eq!(
        recovered.expect("mid-replace read must recover once the file lands"),
        delivered,
        "reader must return the freshly delivered whole-file bytes, never empty/partial"
    );
}

// #1568 root-cause reproduction: many threads rewriting the SAME durable
// status file concurrently must never let a reader observe a corrupt,
// partial, empty, or trailing-garbage state, and must not leak staging temp
// files. Writers alternate between a SHORT and a LONG payload (via a large
// `error_message`) so that the pre-fix shared `status.json.tmp` staging file
// would interleave two `write_all`s — a shorter blob's tail left the longer
// blob's bytes behind, exactly the `trailing characters at line N` corruption
// the daemon logged. With per-write unique staging names this is impossible:
// each writer renames its own complete, fsynced blob into place. The source
// of truth checked here is the ACTUAL on-disk bytes (parsed back), not any
// return value.
#[test]
fn shell_status_concurrent_multiwriter_never_corrupts() {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
        thread,
    };

    const WRITERS: usize = 6;
    const WRITES_PER_WRITER: usize = 300;
    const READERS: usize = 4;

    let temp = tempfile::TempDir::new()
        .unwrap_or_else(|error| panic!("create temp shell status dir: {error}"));
    let paths = ShellJobPaths {
        job_dir: temp.path().to_path_buf(),
        stdout_path: temp.path().join("stdout.log"),
        stderr_path: temp.path().join("stderr.log"),
        status_path: temp.path().join("status.json"),
        request_path: temp.path().join("request.json"),
        remote_cleanup_path: temp.path().join("remote-cleanup.json"),
    };
    let params = ActRunShellStartParams {
        command: "powershell.exe".to_owned(),
        args: vec![
            "-NoProfile".to_owned(),
            "-Command".to_owned(),
            "Write-Output issue1568-mw".to_owned(),
        ],
        working_dir: None,
        env: BTreeMap::new(),
        timeout_ms: None,
        job_id: Some("issue1568-mw".to_owned()),
    };
    let authorization = RunShellAuthorization {
        command_line: shell_command_line_from_parts(&params.command, &params.args),
        matched_pattern: "__any_permitted__".to_owned(),
    };
    let request_sha = run_shell_start_request_sha256(&params)
        .unwrap_or_else(|error| panic!("start request should hash: {error}"));
    let base = shell_job_status_record(
        "issue1568-mw",
        "running",
        &params,
        &paths,
        &request_sha,
        &authorization,
        "2026-07-12T00:00:00Z".to_owned(),
        Some(4242),
        None,
    );
    // Seed the initial complete status so readers always have a whole file.
    write_shell_job_status(&paths.status_path, &base)
        .unwrap_or_else(|error| panic!("initial status should write: {error}"));

    let stop = Arc::new(AtomicBool::new(false));
    let read_errors = Arc::new(AtomicUsize::new(0));
    let reads_ok = Arc::new(AtomicUsize::new(0));

    let readers: Vec<_> = (0..READERS)
        .map(|_| {
            let status_path = paths.status_path.clone();
            let stop = Arc::clone(&stop);
            let read_errors = Arc::clone(&read_errors);
            let reads_ok = Arc::clone(&reads_ok);
            thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    match read_shell_job_status(&status_path, "issue1568-mw") {
                        Ok(status) => {
                            assert_eq!(status.job_id, "issue1568-mw");
                            reads_ok.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(_) => {
                            read_errors.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    thread::yield_now();
                }
            })
        })
        .collect();

    let writes_ok = Arc::new(AtomicUsize::new(0));
    let writes_failed_loud = Arc::new(AtomicUsize::new(0));
    let writes_failed_other = Arc::new(AtomicUsize::new(0));
    let writers: Vec<_> = (0..WRITERS)
        .map(|writer_index| {
            let paths = paths.clone();
            let mut status = base.clone();
            let writes_ok = Arc::clone(&writes_ok);
            let writes_failed_loud = Arc::clone(&writes_failed_loud);
            let writes_failed_other = Arc::clone(&writes_failed_other);
            thread::spawn(move || {
                for iteration in 0..WRITES_PER_WRITER {
                    // Alternate a large vs empty `error_message` so successive
                    // serialized blobs differ sharply in length — the exact
                    // condition that turned a shared staging file into
                    // trailing-garbage before the fix.
                    if (writer_index + iteration) % 2 == 0 {
                        status.status = "running".to_owned();
                        status.error_message = Some("y".repeat(8192));
                    } else {
                        status.status = "finalizing".to_owned();
                        status.error_message = None;
                    }
                    status.duration_ms = Some(iteration as u64);
                    // A rewrite either fully commits (atomic rename) or fails
                    // LOUDLY. Under extreme OS/AV rename contention a commit can
                    // still fail with STORAGE_WRITE_FAILED after its bounded
                    // retries — that is correct fail-loud behavior, NOT
                    // corruption: the atomic rename leaves the on-disk file
                    // untouched, so a concurrent reader still sees the prior whole
                    // status. The #1568 invariant under test is "no reader ever
                    // observes a corrupt/partial file" (asserted below); a loud
                    // write failure is tolerated, a silent/other failure is not.
                    match write_shell_job_status(&paths.status_path, &status) {
                        Ok(()) => {
                            writes_ok.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(error) if error.to_string().contains("STORAGE_WRITE_FAILED") => {
                            writes_failed_loud.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(_) => {
                            writes_failed_other.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
            })
        })
        .collect();

    for writer in writers {
        writer
            .join()
            .unwrap_or_else(|error| panic!("writer thread should join: {error:?}"));
    }
    stop.store(true, Ordering::Relaxed);
    for reader in readers {
        reader
            .join()
            .unwrap_or_else(|error| panic!("reader thread should join: {error:?}"));
    }

    // Source of truth #1: the final on-disk bytes parse as a whole status.
    let final_status = read_shell_job_status(&paths.status_path, "issue1568-mw")
        .unwrap_or_else(|error| panic!("final status must parse: {error}"));
    // Source of truth #2: not a single reader observed a corrupt/partial read
    // across the whole concurrent run.
    let observed_errors = read_errors.load(Ordering::Relaxed);
    let observed_ok = reads_ok.load(Ordering::Relaxed);
    let ok = writes_ok.load(Ordering::Relaxed);
    let failed_loud = writes_failed_loud.load(Ordering::Relaxed);
    let failed_other = writes_failed_other.load(Ordering::Relaxed);
    // Source of truth #3: no staging temp file leaked in the job dir beyond
    // the rare loud write failures whose best-effort cleanup could also lose
    // the AV race (a clean run leaks none).
    let leaked: Vec<String> = fs::read_dir(temp.path())
        .unwrap_or_else(|error| panic!("scan job dir: {error}"))
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains(".tmp."))
        .collect();

    println!(
        "readback=act_run_shell_status edge=concurrent_multiwriter before={WRITERS}x{WRITES_PER_WRITER}_rewrites after=read_errors:{observed_errors} reads_ok:{observed_ok} writes_ok:{ok} writes_failed_loud:{failed_loud} writes_failed_other:{failed_other} final_status:{} leaked_temps:{}",
        final_status.status,
        leaked.len()
    );
    // THE #1568 invariant: atomicity — no reader ever observed a
    // corrupt/partial/empty status across the whole concurrent storm.
    assert_eq!(
        observed_errors, 0,
        "no reader may observe a corrupt/partial/empty status ({observed_errors} did)"
    );
    assert!(observed_ok > 0, "readers must have completed real reads");
    // Every write failure, if any, is a LOUD STORAGE_WRITE_FAILED (old file
    // intact) — never a silent or unexpected failure.
    assert_eq!(
        failed_other, 0,
        "a write failure must surface as a loud STORAGE_WRITE_FAILED, never silent/other ({failed_other} were)"
    );
    assert!(ok > 0, "at least some concurrent writes must commit");
    assert_eq!(final_status.job_id, "issue1568-mw");
    assert!(
        leaked.len() <= failed_loud,
        "staging temp leaks ({}) must not exceed loud write failures ({failed_loud}): {leaked:?}",
        leaked.len()
    );
}

/// Deterministic regression guard for #1608: the durable status reader must ride
/// out a *sustained* transient open failure — an AV/indexer sweep or a peer
/// handle briefly holding `status.json` without share-read — instead of
/// surfacing a spurious error to a status poll, cleanup scan, or dashboard read.
///
/// Before this fix `read_shell_status_bytes` retried on a WALL-CLOCK 500 ms
/// budget; under scheduler starvation (a full parallel test suite) that window
/// could expire after only one or two real open attempts, exactly the class
/// behind the flaky `shell_status_concurrent_multiwriter_never_corrupts` failure
/// (a reader observing a transient error). The reader now retries by ATTEMPT
/// COUNT like the writer's move-retry.
///
/// This reproduces the transient condition deterministically: a peer thread
/// holds the status file open with a NO-SHARING handle, so every concurrent open
/// fails with `ERROR_SHARING_VIOLATION` (32). The lock is held for **650 ms** —
/// deliberately longer than the old 500 ms wall-clock budget — then released.
/// The old reader would give up at 500 ms and fail the read; the attempt-count
/// reader rides it out and returns the whole status. (Windows-only: the POSIX
/// reader path uses atomic `rename(2)` and never sees a transient open error.)
#[cfg(windows)]
#[test]
fn shell_status_reader_rides_out_sustained_transient_lock() {
    use std::{
        os::windows::fs::OpenOptionsExt,
        sync::{Arc, Barrier},
        thread,
        time::Duration,
    };

    let temp = tempfile::TempDir::new()
        .unwrap_or_else(|error| panic!("create temp shell status dir: {error}"));
    let paths = ShellJobPaths {
        job_dir: temp.path().to_path_buf(),
        stdout_path: temp.path().join("stdout.log"),
        stderr_path: temp.path().join("stderr.log"),
        status_path: temp.path().join("status.json"),
        request_path: temp.path().join("request.json"),
        remote_cleanup_path: temp.path().join("remote-cleanup.json"),
    };
    let params = ActRunShellStartParams {
        command: "powershell.exe".to_owned(),
        args: vec![
            "-NoProfile".to_owned(),
            "-Command".to_owned(),
            "Write-Output issue1608-reader".to_owned(),
        ],
        working_dir: None,
        env: BTreeMap::new(),
        timeout_ms: None,
        job_id: Some("issue1608-reader".to_owned()),
    };
    let authorization = RunShellAuthorization {
        command_line: shell_command_line_from_parts(&params.command, &params.args),
        matched_pattern: "__any_permitted__".to_owned(),
    };
    let request_sha = run_shell_start_request_sha256(&params)
        .unwrap_or_else(|error| panic!("start request should hash: {error}"));
    // Seed a complete status through the production writer so the reader has a
    // whole file to recover once the transient lock clears.
    let base = shell_job_status_record(
        "issue1608-reader",
        "running",
        &params,
        &paths,
        &request_sha,
        &authorization,
        "2026-07-12T00:00:00Z".to_owned(),
        Some(4242),
        None,
    );
    write_shell_job_status(&paths.status_path, &base)
        .unwrap_or_else(|error| panic!("seed status should write: {error}"));

    // Deny FILE_SHARE_READ | WRITE | DELETE: while this handle is open, every
    // concurrent open of `status.json` fails with ERROR_SHARING_VIOLATION.
    let barrier = Arc::new(Barrier::new(2));
    let holder_barrier = Arc::clone(&barrier);
    let holder_path = paths.status_path.clone();
    let holder = thread::spawn(move || {
        let locked = OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&holder_path)
            .unwrap_or_else(|error| panic!("peer should exclusively open status: {error}"));
        holder_barrier.wait();
        // Longer than the old 500 ms wall-clock budget so the pre-fix reader
        // would give up; well within the attempt-count budget so the fixed
        // reader recovers.
        thread::sleep(Duration::from_millis(650));
        drop(locked);
    });

    barrier.wait();
    let status =
        read_shell_job_status(&paths.status_path, "issue1608-reader").unwrap_or_else(|error| {
            panic!("reader must ride out a sustained transient lock, not fail: {error}")
        });
    holder
        .join()
        .unwrap_or_else(|error| panic!("holder thread should join: {error:?}"));

    println!(
        "readback=act_run_shell_status edge=sustained_transient_lock after=recovered_status:{status:?}"
    );
    assert_eq!(status.job_id, "issue1608-reader");
    assert_eq!(status.status, "running");
}

#[test]
fn shell_job_reconciliation_preserves_monitor_terminal_status() {
    let temp = tempfile::TempDir::new()
        .unwrap_or_else(|error| panic!("create temp shell status dir: {error}"));
    let paths = ShellJobPaths {
        job_dir: temp.path().to_path_buf(),
        stdout_path: temp.path().join("stdout.log"),
        stderr_path: temp.path().join("stderr.log"),
        status_path: temp.path().join("status.json"),
        request_path: temp.path().join("request.json"),
        remote_cleanup_path: temp.path().join("remote-cleanup.json"),
    };
    let params = ActRunShellStartParams {
        command: "powershell.exe".to_owned(),
        args: vec![
            "-NoProfile".to_owned(),
            "-Command".to_owned(),
            "Write-Output issue970-ok".to_owned(),
        ],
        working_dir: None,
        env: BTreeMap::new(),
        timeout_ms: None,
        job_id: Some("issue970-reconcile".to_owned()),
    };
    let authorization = RunShellAuthorization {
        command_line: shell_command_line_from_parts(&params.command, &params.args),
        matched_pattern: "__any_permitted__".to_owned(),
    };
    let request_sha = run_shell_start_request_sha256(&params)
        .unwrap_or_else(|error| panic!("start request should hash: {error}"));
    let mut terminal = shell_job_status_record(
        "issue970-reconcile",
        "ok",
        &params,
        &paths,
        &request_sha,
        &authorization,
        "2026-06-14T00:00:00Z".to_owned(),
        Some(4242),
        None,
    );
    terminal.exit_code = Some(0);
    terminal.completed_at = Some("2026-06-14T00:00:01Z".to_owned());
    terminal.duration_ms = Some(1000);
    write_shell_job_status(&paths.status_path, &terminal)
        .unwrap_or_else(|error| panic!("terminal status should write: {error}"));

    let mut finalizing = terminal.clone();
    finalizing.status = "finalizing".to_owned();
    finalizing.exit_code = None;
    finalizing.completed_at = Some("2026-06-14T00:00:02Z".to_owned());
    finalizing.duration_ms = Some(2000);

    let preserved = write_shell_job_reconciliation_status(&paths, finalizing)
        .unwrap_or_else(|error| panic!("reconcile write should preserve terminal: {error}"));
    let readback = read_shell_job_status(&paths.status_path, "issue970-reconcile")
        .unwrap_or_else(|error| panic!("status should read: {error}"));

    println!(
        "readback=act_run_shell_status edge=terminal_preservation before=candidate:finalizing after=file_status:{} exit_code:{:?}",
        readback.status, readback.exit_code
    );
    assert_eq!(preserved.status, "ok");
    assert_eq!(preserved.exit_code, Some(0));
    assert_eq!(readback.status, "ok");
    assert_eq!(readback.exit_code, Some(0));

    let mut exited_unobserved = terminal.clone();
    exited_unobserved.status = "exited_unobserved".to_owned();
    exited_unobserved.exit_code = None;
    exited_unobserved.error_code = Some(error_codes::TOOL_INTERNAL_ERROR.to_owned());
    exited_unobserved.error_message =
        Some("job process exited before the monitor persisted final status".to_owned());
    let preserved_after_unobserved =
        write_shell_job_reconciliation_status(&paths, exited_unobserved).unwrap_or_else(|error| {
            panic!("reconcile write should not downgrade terminal: {error}")
        });
    let readback_after_unobserved = read_shell_job_status(&paths.status_path, "issue970-reconcile")
        .unwrap_or_else(|error| panic!("status should read after unobserved: {error}"));

    println!(
        "readback=act_run_shell_status edge=terminal_preservation before=candidate:exited_unobserved after=file_status:{} exit_code:{:?}",
        readback_after_unobserved.status, readback_after_unobserved.exit_code
    );
    assert_eq!(preserved_after_unobserved.status, "ok");
    assert_eq!(preserved_after_unobserved.exit_code, Some(0));
    assert_eq!(readback_after_unobserved.status, "ok");
    assert_eq!(readback_after_unobserved.exit_code, Some(0));
}

// #1334: a durable job whose status still claims "running" but whose backing
// process is dead must be reconciled off the live set, not retained forever.
// Source of truth = the persisted status file on disk after reconcile.
#[test]
fn reconcile_demotes_running_job_with_dead_pid_off_live_set() {
    let temp = tempfile::TempDir::new()
        .unwrap_or_else(|error| panic!("create temp shell status dir: {error}"));
    let paths = ShellJobPaths {
        job_dir: temp.path().to_path_buf(),
        stdout_path: temp.path().join("stdout.log"),
        stderr_path: temp.path().join("stderr.log"),
        status_path: temp.path().join("status.json"),
        request_path: temp.path().join("request.json"),
        remote_cleanup_path: temp.path().join("remote-cleanup.json"),
    };
    let params = ActRunShellStartParams {
        command: "powershell.exe".to_owned(),
        args: vec![
            "-NoProfile".to_owned(),
            "-Command".to_owned(),
            "Start-Sleep -Seconds 600".to_owned(),
        ],
        working_dir: None,
        env: BTreeMap::new(),
        timeout_ms: None,
        job_id: Some("issue1334-phantom".to_owned()),
    };
    let authorization = RunShellAuthorization {
        command_line: shell_command_line_from_parts(&params.command, &params.args),
        matched_pattern: "__any_permitted__".to_owned(),
    };
    let request_sha = run_shell_start_request_sha256(&params)
        .unwrap_or_else(|error| panic!("start request should hash: {error}"));
    // A PID that cannot be alive (max u32, never a real Windows PID).
    let dead_pid = u32::MAX - 1;
    let phantom = shell_job_status_record(
        "issue1334-phantom",
        "running",
        &params,
        &paths,
        &request_sha,
        &authorization,
        "2026-06-14T00:00:00Z".to_owned(),
        Some(dead_pid),
        None,
    );
    write_shell_job_status(&paths.status_path, &phantom)
        .unwrap_or_else(|error| panic!("plant phantom status: {error}"));

    // Precondition: status string alone classifies it live (the old bug).
    assert!(shell_job_live_status(&phantom.status));
    // But PID-backed liveness already knows it is dead.
    assert!(!shell_job_process_still_running(&phantom));

    let reconciled = reconcile_shell_job_process_state(phantom, &paths)
        .unwrap_or_else(|error| panic!("reconcile should succeed: {error}"));
    println!(
        "readback=reconcile edge=running_dead_pid before=running after=status:{}",
        reconciled.status
    );
    assert_ne!(reconciled.status, "running");
    assert!(!shell_job_process_still_running(&reconciled));

    // Source of truth: re-read the persisted file — the phantom is no longer
    // a live "running" job on disk.
    let on_disk = read_shell_job_status(&paths.status_path, "issue1334-phantom")
        .unwrap_or_else(|error| panic!("status should read after reconcile: {error}"));
    assert_ne!(on_disk.status, "running");
    assert!(!shell_job_live_status(&on_disk.status));
}

#[test]
fn shell_job_status_readback_preserves_terminal_monitor_status() {
    let temp = tempfile::TempDir::new()
        .unwrap_or_else(|error| panic!("create temp shell status dir: {error}"));
    let paths = ShellJobPaths {
        job_dir: temp.path().to_path_buf(),
        stdout_path: temp.path().join("stdout.log"),
        stderr_path: temp.path().join("stderr.log"),
        status_path: temp.path().join("status.json"),
        request_path: temp.path().join("request.json"),
        remote_cleanup_path: temp.path().join("remote-cleanup.json"),
    };
    std::fs::write(&paths.stdout_path, b"issue989-ok\r\n")
        .unwrap_or_else(|error| panic!("stdout should write: {error}"));
    std::fs::write(&paths.stderr_path, b"")
        .unwrap_or_else(|error| panic!("stderr should write: {error}"));
    let params = ActRunShellStartParams {
        command: "powershell.exe".to_owned(),
        args: vec![
            "-NoProfile".to_owned(),
            "-Command".to_owned(),
            "Write-Output issue989-ok".to_owned(),
        ],
        working_dir: None,
        env: BTreeMap::new(),
        timeout_ms: None,
        job_id: Some("issue989-status-readback".to_owned()),
    };
    let authorization = RunShellAuthorization {
        command_line: shell_command_line_from_parts(&params.command, &params.args),
        matched_pattern: "__any_permitted__".to_owned(),
    };
    let request_sha = run_shell_start_request_sha256(&params)
        .unwrap_or_else(|error| panic!("start request should hash: {error}"));
    let mut terminal = shell_job_status_record(
        "issue989-status-readback",
        "ok",
        &params,
        &paths,
        &request_sha,
        &authorization,
        "2026-06-14T00:00:00Z".to_owned(),
        Some(4242),
        None,
    );
    terminal.exit_code = Some(0);
    terminal.completed_at = Some("2026-06-14T00:00:01Z".to_owned());
    terminal.duration_ms = Some(1000);
    write_shell_job_status(&paths.status_path, &terminal)
        .unwrap_or_else(|error| panic!("terminal status should write: {error}"));

    let mut stale_finalizing = terminal.clone();
    stale_finalizing.status = "finalizing".to_owned();
    stale_finalizing.exit_code = None;
    stale_finalizing.completed_at = Some("2026-06-14T00:00:02Z".to_owned());
    stale_finalizing.duration_ms = Some(2000);
    stale_finalizing.diagnostics = Some(shell_job_status_diagnostics(
        &stale_finalizing,
        false,
        13,
        0,
    ));

    let (persisted, running) =
        write_shell_job_status_readback(&paths, stale_finalizing, false, 13, 0)
            .unwrap_or_else(|error| panic!("status readback should preserve terminal: {error}"));
    let readback = read_shell_job_status(&paths.status_path, "issue989-status-readback")
        .unwrap_or_else(|error| panic!("status should read after readback write: {error}"));

    println!(
        "readback=act_run_shell_status edge=diagnostic_write_race before=candidate:finalizing/null-exit after=file_status:{} exit_code:{:?} diagnostics:{}",
        readback.status,
        readback.exit_code,
        readback.diagnostics.is_some()
    );
    assert!(!running);
    assert_eq!(persisted.status, "ok");
    assert_eq!(persisted.exit_code, Some(0));
    assert!(persisted.diagnostics.is_some());
    assert_eq!(readback.status, "ok");
    assert_eq!(readback.exit_code, Some(0));
    assert!(readback.diagnostics.is_some());
}

#[test]
fn shell_monitor_persists_terminal_status_before_remote_cleanup() {
    let temp = tempfile::TempDir::new()
        .unwrap_or_else(|error| panic!("create temp shell status dir: {error}"));
    let paths = ShellJobPaths {
        job_dir: temp.path().to_path_buf(),
        stdout_path: temp.path().join("stdout.log"),
        stderr_path: temp.path().join("stderr.log"),
        status_path: temp.path().join("status.json"),
        request_path: temp.path().join("request.json"),
        remote_cleanup_path: temp.path().join("remote-cleanup.json"),
    };
    let params = ActRunShellStartParams {
        command: "ssh.exe".to_owned(),
        args: vec!["aiwonder".to_owned(), "printf issue1244-ok".to_owned()],
        working_dir: None,
        env: BTreeMap::new(),
        timeout_ms: None,
        job_id: Some("issue1244-local-terminal-before-cleanup".to_owned()),
    };
    let authorization = RunShellAuthorization {
        command_line: "ssh.exe aiwonder 'printf issue1244-ok'".to_owned(),
        matched_pattern: "__any_permitted__".to_owned(),
    };
    let request_sha = run_shell_start_request_sha256(&params)
        .unwrap_or_else(|error| panic!("start request should hash: {error}"));
    let mut terminal = shell_job_status_record(
        "issue1244-local-terminal-before-cleanup",
        "ok",
        &params,
        &paths,
        &request_sha,
        &authorization,
        "2026-06-18T00:00:00Z".to_owned(),
        Some(4242),
        None,
    );
    let metadata =
        parse_remote_process_metadata(
            "SYNAPSE_REMOTE_PROCESS_V1 job_id=issue1244-local-terminal-before-cleanup pid=12345 pgid=12345 sid=12345\n",
            "issue1244-local-terminal-before-cleanup",
        )
        .unwrap_or_else(|| panic!("remote marker should parse"));
    apply_remote_process_metadata(&mut terminal, metadata);
    terminal.exit_code = Some(0);
    terminal.completed_at = Some("2026-06-18T00:00:01Z".to_owned());
    terminal.duration_ms = Some(1000);

    let mut stale_finalizing = terminal.clone();
    stale_finalizing.status = "finalizing".to_owned();
    stale_finalizing.exit_code = None;
    stale_finalizing.completed_at = Some("2026-06-18T00:00:02Z".to_owned());
    stale_finalizing.duration_ms = Some(2000);
    write_shell_job_status(&paths.status_path, &stale_finalizing)
        .unwrap_or_else(|error| panic!("stale finalizing status should write: {error}"));

    persist_shell_job_local_terminal_status(&paths, &terminal);
    let readback = read_shell_job_status(
        &paths.status_path,
        "issue1244-local-terminal-before-cleanup",
    )
    .unwrap_or_else(|error| panic!("status should read after local terminal prewrite: {error}"));

    println!(
        "readback=act_run_shell_start edge=local_terminal_pre_remote_cleanup before=status:finalizing exit_code:None remote:{} after=status:{} exit_code:{:?} remote:{}",
        stale_finalizing.remote_process_scope.remote_cleanup_status,
        readback.status,
        readback.exit_code,
        readback.remote_process_scope.remote_cleanup_status
    );
    assert_eq!(readback.status, "ok");
    assert_eq!(readback.exit_code, Some(0));
    assert_eq!(
        readback.remote_process_scope.remote_cleanup_status,
        SHELL_REMOTE_CLEANUP_TRACKED
    );
    assert!(!readback.remote_process_scope.remote_cleanup_verified);
}

#[test]
fn shell_reconciliation_preserves_successful_terminal_status_against_late_nonzero_candidate() {
    let temp = tempfile::TempDir::new()
        .unwrap_or_else(|error| panic!("create temp shell status dir: {error}"));
    let paths = ShellJobPaths {
        job_dir: temp.path().to_path_buf(),
        stdout_path: temp.path().join("stdout.log"),
        stderr_path: temp.path().join("stderr.log"),
        status_path: temp.path().join("status.json"),
        request_path: temp.path().join("request.json"),
        remote_cleanup_path: temp.path().join("remote-cleanup.json"),
    };
    let params = ActRunShellStartParams {
        command: "ssh.exe".to_owned(),
        args: vec!["aiwonder".to_owned(), "printf issue1251-ok".to_owned()],
        working_dir: None,
        env: BTreeMap::new(),
        timeout_ms: None,
        job_id: Some("issue1251-preserve-ok".to_owned()),
    };
    let authorization = RunShellAuthorization {
        command_line: "ssh.exe aiwonder 'printf issue1251-ok'".to_owned(),
        matched_pattern: "__any_permitted__".to_owned(),
    };
    let request_sha = run_shell_start_request_sha256(&params)
        .unwrap_or_else(|error| panic!("start request should hash: {error}"));
    let mut latest_ok = shell_job_status_record(
        "issue1251-preserve-ok",
        "ok",
        &params,
        &paths,
        &request_sha,
        &authorization,
        "2026-06-18T00:00:00Z".to_owned(),
        Some(4242),
        None,
    );
    let metadata = parse_remote_process_metadata(
        "SYNAPSE_REMOTE_PROCESS_V1 job_id=issue1251-preserve-ok pid=12345 pgid=12345 sid=12345\n",
        "issue1251-preserve-ok",
    )
    .unwrap_or_else(|| panic!("remote marker should parse"));
    apply_remote_process_metadata(&mut latest_ok, metadata);
    latest_ok.remote_process_scope.remote_cleanup_verified = true;
    latest_ok.remote_process_scope.remote_cleanup_status = SHELL_REMOTE_CLEANUP_VERIFIED.to_owned();
    latest_ok.exit_code = Some(0);
    latest_ok.completed_at = Some("2026-06-18T00:00:01Z".to_owned());
    latest_ok.duration_ms = Some(1000);
    write_shell_job_status(&paths.status_path, &latest_ok)
        .unwrap_or_else(|error| panic!("latest ok status should write: {error}"));

    let mut late_nonzero = latest_ok.clone();
    late_nonzero.status = "exit_nonzero".to_owned();
    late_nonzero.exit_code = Some(1);
    late_nonzero.completed_at = Some("2026-06-18T00:00:02Z".to_owned());
    late_nonzero.duration_ms = Some(2000);
    late_nonzero.remote_process_scope.remote_cleanup_message = Some(
        "late cleanup/readback candidate must not downgrade the remote command verdict".to_owned(),
    );

    let persisted = write_shell_job_reconciliation_status(&paths, late_nonzero)
        .unwrap_or_else(|error| panic!("status readback should preserve success: {error}"));
    let readback = read_shell_job_status(&paths.status_path, "issue1251-preserve-ok")
        .unwrap_or_else(|error| panic!("status should read after reconciliation: {error}"));

    println!(
        "readback=act_run_shell_status edge=preserve_successful_terminal before=file_status:ok exit_code:0 candidate:exit_nonzero/1 after=file_status:{} exit_code:{:?} remote:{}",
        readback.status, readback.exit_code, readback.remote_process_scope.remote_cleanup_status
    );
    assert_eq!(persisted.status, "ok");
    assert_eq!(persisted.exit_code, Some(0));
    assert_eq!(readback.status, "ok");
    assert_eq!(readback.exit_code, Some(0));
    assert_eq!(
        readback.remote_process_scope.remote_cleanup_status,
        SHELL_REMOTE_CLEANUP_VERIFIED
    );
}

#[test]
fn shell_remote_scope_classifies_direct_ssh_with_destination() {
    let args = vec![
        "-o".to_owned(),
        "BatchMode=yes".to_owned(),
        "-p".to_owned(),
        "22".to_owned(),
        "aiwonder".to_owned(),
        "sleep".to_owned(),
        "60".to_owned(),
    ];

    let scope = ssh_remote_process_scope(
        r"C:\Windows\System32\OpenSSH\ssh.exe",
        &args,
        "regression_direct",
    );

    println!(
        "readback=act_run_shell_remote_scope edge=direct_ssh before=args:{args:?} after={scope:?}"
    );
    assert_eq!(scope.transport, SHELL_REMOTE_TRANSPORT_SSH);
    assert_eq!(scope.remote_identity.as_deref(), Some("aiwonder"));
    assert!(scope.remote_cleanup_required);
    assert!(!scope.remote_cleanup_verified);
    assert_eq!(
        scope.remote_cleanup_status,
        SHELL_REMOTE_CLEANUP_TRACKING_PENDING
    );
    assert!(
        scope
            .detection_evidence
            .iter()
            .any(|evidence| evidence.contains(SHELL_REMOTE_PROCESS_MARKER))
    );
}

#[test]
fn shell_remote_scope_ssh_option_parser_is_case_sensitive() {
    let background_args = vec!["-f".to_owned(), "aiwonder".to_owned()];
    let config_args = vec![
        "-F".to_owned(),
        r"C:\tmp\ssh_config".to_owned(),
        "aiwonder".to_owned(),
    ];

    let background_identity = ssh_remote_identity(&background_args);
    let config_identity = ssh_remote_identity(&config_args);

    println!(
        "readback=act_run_shell_remote_scope edge=ssh_option_case before=-f:{background_args:?},-F:{config_args:?} after=-f:{background_identity:?},-F:{config_identity:?}"
    );
    assert_eq!(background_identity.as_deref(), Some("aiwonder"));
    assert_eq!(config_identity.as_deref(), Some("aiwonder"));
}

#[test]
fn shell_remote_tracking_plan_wraps_direct_ssh_remote_command() {
    let args = vec![
        "-o".to_owned(),
        "BatchMode=yes".to_owned(),
        "aiwonder".to_owned(),
        "bash -lc 'exec -a synapse940 sleep 60'".to_owned(),
    ];

    let plan = ssh_remote_tracking_plan("ssh.exe", &args, "issue940-track")
        .expect("direct ssh remote command should be tracking-capable");

    println!(
        "readback=act_run_shell_remote_tracking edge=wrap before=args:{args:?} after={plan:?}"
    );
    assert_eq!(plan.remote_identity, "aiwonder");
    assert_eq!(
        plan.remote_command,
        "bash -lc 'exec -a synapse940 sleep 60'"
    );
    assert_eq!(plan.spawn_args[0], "-o");
    assert_eq!(plan.spawn_args[1], "BatchMode=yes");
    assert_eq!(plan.spawn_args[2], "aiwonder");
    let remote_wrapper = plan
        .spawn_args
        .last()
        .expect("wrapper command should be appended after destination");
    assert!(remote_wrapper.contains("setsid sh -c"));
    assert!(remote_wrapper.contains("SYNAPSE_REMOTE_PROCESS_V1 job_id=issue940-track"));
    assert!(remote_wrapper.contains("bash -lc"));
}

#[test]
fn shell_wrapped_powershell_ssh_remote_command_is_tracked() {
    let args = vec![
        "-NoLogo".to_owned(),
        "-NoProfile".to_owned(),
        "-NonInteractive".to_owned(),
        "-Command".to_owned(),
        "ssh -o BatchMode=yes aiwonder \"cd /repo/calyx && cargo test -p calyx-aster --test soak_ph58 -- --nocapture --test-threads=1\""
            .to_owned(),
    ];
    let params = ActRunShellStartParams {
        command: "powershell.exe".to_owned(),
        args: args.clone(),
        working_dir: None,
        env: BTreeMap::new(),
        timeout_ms: None,
        job_id: Some("issue1019-powershell-ssh".to_owned()),
    };

    let invocation = shell_job_ssh_command_invocation(&params.command, &params.args)
        .expect("single PowerShell SSH command should be parseable");
    let scope = shell_job_remote_process_scope_from_start_params(&params);
    let spawn_plan = shell_job_spawn_plan(&params, "issue1019-powershell-ssh");

    println!(
        "readback=act_run_shell_remote_tracking edge=powershell_ssh before=command:{} args:{args:?} after=invocation:{invocation:?} scope:{scope:?} spawn:{spawn_plan:?}",
        params.command
    );
    assert_eq!(invocation.command, "ssh");
    assert_eq!(invocation.evidence, "shell_wrapped_ssh:powershell");
    assert_eq!(scope.transport, SHELL_REMOTE_TRANSPORT_SSH);
    assert_eq!(scope.remote_identity.as_deref(), Some("aiwonder"));
    assert_eq!(
        scope.remote_cleanup_status,
        SHELL_REMOTE_CLEANUP_TRACKING_PENDING
    );
    assert!(
        scope
            .detection_evidence
            .iter()
            .any(|evidence| evidence.contains("shell_wrapped_ssh:powershell"))
    );
    assert_eq!(spawn_plan.command, "ssh");
    assert!(spawn_plan.args.last().is_some_and(|arg| {
        arg.contains("SYNAPSE_REMOTE_PROCESS_V1 job_id=issue1019-powershell-ssh")
    }));
}

#[test]
fn shell_wrapped_complex_powershell_script_is_not_guessed_as_trackable_ssh() {
    let args = vec![
        "-NoProfile".to_owned(),
        "-Command".to_owned(),
        "Write-Output before; ssh aiwonder sleep 60".to_owned(),
    ];
    let params = ActRunShellStartParams {
        command: "powershell.exe".to_owned(),
        args: args.clone(),
        working_dir: None,
        env: BTreeMap::new(),
        timeout_ms: None,
        job_id: Some("issue1019-complex-powershell".to_owned()),
    };

    let invocation = shell_job_ssh_command_invocation(&params.command, &params.args);
    let scope = shell_job_remote_process_scope_from_start_params(&params);
    let spawn_plan = shell_job_spawn_plan(&params, "issue1019-complex-powershell");

    println!(
        "readback=act_run_shell_remote_tracking edge=complex_powershell before=command:{} args:{args:?} after=invocation:{invocation:?} scope:{scope:?} spawn:{spawn_plan:?}",
        params.command
    );
    assert!(invocation.is_none());
    assert_eq!(scope.transport, SHELL_REMOTE_TRANSPORT_LOCAL);
    assert_eq!(
        scope.remote_cleanup_status,
        SHELL_REMOTE_CLEANUP_NOT_APPLICABLE
    );
    assert_eq!(spawn_plan.command, "powershell.exe");
    assert_eq!(spawn_plan.args, args);
}

#[test]
fn shell_wrapped_powershell_ssh_with_escaped_remote_quotes_is_not_rewritten() {
    let script = "ssh -o BatchMode=yes -i //wsl.localhost/Ubuntu-24.04/home/cabdru/.ssh/id_ed25519 -l croyse aiwonder \"sh -lc 'd=$HOME/synapse_issue1259; mkdir -p \\\"$d\\\"; printf 0 > \\\"$d/remote.rc\\\"; exit 0'\"";
    let args = vec![
        "-NoProfile".to_owned(),
        "-Command".to_owned(),
        script.to_owned(),
    ];
    let params = ActRunShellStartParams {
        command: "powershell.exe".to_owned(),
        args: args.clone(),
        working_dir: None,
        env: BTreeMap::new(),
        timeout_ms: None,
        job_id: Some("issue1259-escaped-powershell".to_owned()),
    };

    let invocation = shell_job_ssh_command_invocation(&params.command, &params.args);
    let scope = shell_job_remote_process_scope_from_start_params(&params);
    let spawn_plan = shell_job_spawn_plan(&params, "issue1259-escaped-powershell");

    println!(
        "readback=act_run_shell_remote_tracking edge=escaped_powershell_quotes before=script:{script:?} after=invocation:{invocation:?} scope:{scope:?} spawn:{spawn_plan:?}"
    );
    assert!(invocation.is_none());
    assert_eq!(scope.transport, SHELL_REMOTE_TRANSPORT_LOCAL);
    assert_eq!(
        scope.remote_cleanup_status,
        SHELL_REMOTE_CLEANUP_NOT_APPLICABLE
    );
    assert_eq!(spawn_plan.command, "powershell.exe");
    assert_eq!(spawn_plan.args, args);
}

#[test]
fn shell_remote_tracking_plan_refuses_ssh_modes_without_cleanup_handle() {
    let forwarding = vec![
        "-N".to_owned(),
        "-L".to_owned(),
        "127.0.0.1:1:127.0.0.1:1".to_owned(),
        "aiwonder".to_owned(),
    ];
    let subsystem = vec!["-s".to_owned(), "aiwonder".to_owned(), "sftp".to_owned()];

    let forwarding_plan = ssh_remote_tracking_plan("ssh.exe", &forwarding, "issue940-forward");
    let subsystem_plan = ssh_remote_tracking_plan("ssh.exe", &subsystem, "issue940-subsystem");
    let subsystem_scope = ssh_remote_process_scope("ssh.exe", &subsystem, "regression_subsystem");

    println!(
        "readback=act_run_shell_remote_tracking edge=unsupported before=-N:{forwarding:?},-s:{subsystem:?} after=-N:{forwarding_plan:?},-s:{subsystem_plan:?},scope:{subsystem_scope:?}"
    );
    assert!(forwarding_plan.is_none());
    assert!(subsystem_plan.is_none());
    assert_eq!(
        subsystem_scope.remote_cleanup_status,
        SHELL_REMOTE_CLEANUP_NOT_TRACKED
    );
    assert!(
        subsystem_scope
            .detection_evidence
            .iter()
            .any(|evidence| evidence.contains("remote_tracking_unsupported"))
    );
}

#[test]
fn shell_terminal_tracking_pending_without_marker_is_loudly_unverified() {
    let temp = tempfile::TempDir::new()
        .unwrap_or_else(|error| panic!("create temp shell status dir: {error}"));
    let paths = ShellJobPaths {
        job_dir: temp.path().to_path_buf(),
        stdout_path: temp.path().join("stdout.log"),
        stderr_path: temp.path().join("stderr.log"),
        status_path: temp.path().join("status.json"),
        request_path: temp.path().join("request.json"),
        remote_cleanup_path: temp.path().join("remote-cleanup.json"),
    };
    std::fs::write(&paths.stdout_path, b"")
        .unwrap_or_else(|error| panic!("write stdout log: {error}"));
    std::fs::write(&paths.stderr_path, b"")
        .unwrap_or_else(|error| panic!("write stderr log: {error}"));
    let params = ActRunShellStartParams {
        command: "ssh.exe".to_owned(),
        args: vec!["aiwonder".to_owned(), "true".to_owned()],
        working_dir: None,
        env: BTreeMap::new(),
        timeout_ms: None,
        job_id: Some("issue972-no-marker".to_owned()),
    };
    let authorization = RunShellAuthorization {
        command_line: "ssh.exe aiwonder true".to_owned(),
        matched_pattern: "^ssh".to_owned(),
    };
    let mut status = shell_job_status_record(
        "issue972-no-marker",
        "ok",
        &params,
        &paths,
        "request-sha",
        &authorization,
        "2026-06-14T00:00:00Z".to_owned(),
        Some(1234),
        None,
    );

    verify_shell_job_remote_cleanup_after_terminal(
        &mut status,
        &paths,
        "regression_terminal_readback",
        None,
    );

    println!(
        "readback=act_run_shell_remote_cleanup edge=terminal_no_marker before=tracking_pending after={:?}",
        status.remote_process_scope
    );
    assert_eq!(
        status.remote_process_scope.remote_cleanup_status,
        SHELL_REMOTE_CLEANUP_UNVERIFIED
    );
    assert_eq!(
        status.error_code.as_deref(),
        Some(error_codes::ACTION_REMOTE_PROCESS_CLEANUP_UNVERIFIED)
    );
    assert!(!status.remote_process_scope.remote_cleanup_verified);
}

#[test]
fn shell_terminal_pre_marker_parse_failure_is_classified_without_cleanup_unverified() {
    let temp = tempfile::TempDir::new()
        .unwrap_or_else(|error| panic!("create temp shell status dir: {error}"));
    let paths = ShellJobPaths {
        job_dir: temp.path().to_path_buf(),
        stdout_path: temp.path().join("stdout.log"),
        stderr_path: temp.path().join("stderr.log"),
        status_path: temp.path().join("status.json"),
        request_path: temp.path().join("request.json"),
        remote_cleanup_path: temp.path().join("remote-cleanup.json"),
    };
    let stderr = "bash: -c: line 1: unexpected EOF while looking for matching `''\n";
    std::fs::write(&paths.stdout_path, b"")
        .unwrap_or_else(|error| panic!("write stdout log: {error}"));
    std::fs::write(&paths.stderr_path, stderr)
        .unwrap_or_else(|error| panic!("write stderr log: {error}"));
    let params = ActRunShellStartParams {
        command: "ssh.exe".to_owned(),
        args: vec![
            "-p".to_owned(),
            "22231".to_owned(),
            "-i".to_owned(),
            "issue1231_key".to_owned(),
            "aiwonder".to_owned(),
            "bash -lc 'echo issue1231".to_owned(),
        ],
        working_dir: None,
        env: BTreeMap::new(),
        timeout_ms: None,
        job_id: Some("issue1231-pre-marker-parse".to_owned()),
    };
    let authorization = RunShellAuthorization {
        command_line: "ssh.exe -p 22231 -i issue1231_key aiwonder \"bash -lc 'echo issue1231\""
            .to_owned(),
        matched_pattern: "^ssh".to_owned(),
    };
    let mut status = shell_job_status_record(
        "issue1231-pre-marker-parse",
        "exit_nonzero",
        &params,
        &paths,
        "request-sha",
        &authorization,
        "2026-06-17T00:00:00Z".to_owned(),
        Some(1234),
        None,
    );
    status.exit_code = Some(2);

    verify_shell_job_remote_cleanup_after_terminal(
        &mut status,
        &paths,
        "regression_terminal_readback",
        None,
    );

    println!(
        "readback=act_run_shell_remote_cleanup edge=pre_marker_parse before=tracking_pending stderr={stderr:?} after={:?}",
        status.remote_process_scope
    );
    assert_eq!(
        status.remote_process_scope.remote_cleanup_status,
        SHELL_REMOTE_CLEANUP_PRE_MARKER_TERMINAL
    );
    assert!(!status.remote_process_scope.remote_cleanup_required);
    assert!(!status.remote_process_scope.remote_cleanup_verified);
    assert_eq!(status.remote_process_scope.remote_cleanup_error_code, None);
    assert_eq!(status.error_code, None);
    assert!(
        status
            .remote_process_scope
            .remote_cleanup_message
            .as_deref()
            .is_some_and(|message| message
                .contains("suggested_safe_readback=ssh.exe -p 22231 -i issue1231_key aiwonder"))
    );
    assert!(
        status
            .remote_process_scope
            .detection_evidence
            .iter()
            .any(|evidence| evidence
                == "remote_tracking_pre_marker_terminal:remote_shell_unmatched_quote")
    );
}

#[test]
fn shell_terminal_not_tracked_ssh_status_is_preserved() {
    let temp = tempfile::TempDir::new()
        .unwrap_or_else(|error| panic!("create temp shell status dir: {error}"));
    let paths = ShellJobPaths {
        job_dir: temp.path().to_path_buf(),
        stdout_path: temp.path().join("stdout.log"),
        stderr_path: temp.path().join("stderr.log"),
        status_path: temp.path().join("status.json"),
        request_path: temp.path().join("request.json"),
        remote_cleanup_path: temp.path().join("remote-cleanup.json"),
    };
    std::fs::write(&paths.stdout_path, b"")
        .unwrap_or_else(|error| panic!("write stdout log: {error}"));
    std::fs::write(&paths.stderr_path, b"")
        .unwrap_or_else(|error| panic!("write stderr log: {error}"));
    let params = ActRunShellStartParams {
        command: "ssh.exe".to_owned(),
        args: vec!["-N".to_owned(), "aiwonder".to_owned()],
        working_dir: None,
        env: BTreeMap::new(),
        timeout_ms: None,
        job_id: Some("issue972-not-tracked".to_owned()),
    };
    let authorization = RunShellAuthorization {
        command_line: "ssh.exe -N aiwonder".to_owned(),
        matched_pattern: "^ssh".to_owned(),
    };
    let mut status = shell_job_status_record(
        "issue972-not-tracked",
        "ok",
        &params,
        &paths,
        "request-sha",
        &authorization,
        "2026-06-14T00:00:00Z".to_owned(),
        Some(1234),
        None,
    );

    verify_shell_job_remote_cleanup_after_terminal(
        &mut status,
        &paths,
        "regression_terminal_readback",
        None,
    );

    println!(
        "readback=act_run_shell_remote_cleanup edge=terminal_not_tracked before=not_tracked after={:?}",
        status.remote_process_scope
    );
    assert_eq!(
        status.remote_process_scope.remote_cleanup_status,
        SHELL_REMOTE_CLEANUP_NOT_TRACKED
    );
    assert!(status.error_code.is_none());
}

#[test]
fn issue1277_shell_terminal_tracked_ssh_transport_reset_defers_remote_cleanup() {
    let temp = tempfile::TempDir::new()
        .unwrap_or_else(|error| panic!("create temp shell status dir: {error}"));
    let paths = temp_shell_job_paths(&temp);
    let stderr = "noise before marker\n\
SYNAPSE_REMOTE_PROCESS_V1 job_id=issue1277-reset pid=3487519 pgid=3487519 sid=3487519\n\
Read from remote host aiwonder.mst.com: Connection reset by peer\r\n\
client_loop: send disconnect: Connection reset by peer\r\n";
    std::fs::write(&paths.stdout_path, b"")
        .unwrap_or_else(|error| panic!("write stdout log: {error}"));
    std::fs::write(&paths.stderr_path, stderr)
        .unwrap_or_else(|error| panic!("write stderr log: {error}"));
    let mut status = issue1277_ssh_status("issue1277-reset", "exit_nonzero", &paths);
    status.exit_code = Some(255);

    verify_shell_job_remote_cleanup_after_terminal(
        &mut status,
        &paths,
        "regression_terminal_readback",
        None,
    );

    println!(
        "readback=act_run_shell_remote_cleanup issue=1277 edge=transport_reset before=status:exit_nonzero exit_code:255 stderr={stderr:?} after=status:{} remote:{:?}",
        status.status, status.remote_process_scope
    );
    assert_eq!(status.status, SHELL_JOB_STATUS_REMOTE_TRANSPORT_LOST);
    assert_eq!(
        status.remote_process_scope.remote_cleanup_status,
        SHELL_REMOTE_CLEANUP_TRANSPORT_LOST
    );
    assert!(status.remote_process_scope.remote_cleanup_required);
    assert!(!status.remote_process_scope.remote_cleanup_verified);
    assert_eq!(
        status.remote_process_scope.remote_process_id.as_deref(),
        Some("3487519")
    );
    assert_eq!(
        status
            .remote_process_scope
            .remote_process_group_id
            .as_deref(),
        Some("3487519")
    );
    assert_eq!(status.remote_process_scope.remote_cleanup_error_code, None);
    assert_eq!(status.error_code, None);
    assert!(
        status
            .remote_process_scope
            .remote_cleanup_message
            .as_deref()
            .is_some_and(|message| message.contains("act_run_shell_cancel"))
    );
    assert!(
        status
            .remote_process_scope
            .detection_evidence
            .iter()
            .any(|evidence| evidence == "remote_transport_lost:ssh_connection_reset")
    );
    assert!(
        status
            .remote_process_scope
            .detection_evidence
            .iter()
            .any(|evidence| evidence == "remote_cleanup_deferred_until_explicit_cancel")
    );

    let after_first_readback =
        serde_json::to_value(&status.remote_process_scope).expect("remote scope serializes");
    verify_shell_job_remote_cleanup_after_terminal(
        &mut status,
        &paths,
        "regression_terminal_readback",
        None,
    );
    assert_eq!(
        serde_json::to_value(&status.remote_process_scope).expect("remote scope serializes"),
        after_first_readback
    );
}

#[test]
fn issue1277_shell_transport_loss_detection_skips_cancel_timeout_and_unrelated_exit_255() {
    let temp = tempfile::TempDir::new()
        .unwrap_or_else(|error| panic!("create temp shell status dir: {error}"));
    let paths = temp_shell_job_paths(&temp);
    let stderr = "SYNAPSE_REMOTE_PROCESS_V1 job_id=issue1277-guard pid=3487519 pgid=3487519 sid=3487519\n\
client_loop: send disconnect: Connection reset by peer\r\n";
    std::fs::write(&paths.stdout_path, b"")
        .unwrap_or_else(|error| panic!("write stdout log: {error}"));
    std::fs::write(&paths.stderr_path, stderr)
        .unwrap_or_else(|error| panic!("write stderr log: {error}"));
    let mut base = issue1277_ssh_status("issue1277-guard", "exit_nonzero", &paths);
    base.exit_code = Some(255);
    let metadata = parse_remote_process_metadata(stderr, "issue1277-guard")
        .expect("remote marker should parse");
    apply_remote_process_metadata(&mut base, metadata);

    let closed_by_remote =
        remote_transport_lost_evidence("Connection to aiwonder.mst.com closed by remote host.\r\n")
            .expect("OpenSSH closed-by-remote-host stderr must classify as transport loss");
    assert_eq!(closed_by_remote.reason, "ssh_closed_by_remote_host");

    let mut cancel_requested = base.clone();
    cancel_requested.cancel_requested = true;
    let mut timed_out = base.clone();
    timed_out.timed_out = true;
    let mut non_255_exit = base.clone();
    non_255_exit.exit_code = Some(1);
    let mut successful_status = base.clone();
    successful_status.status = "ok".to_owned();
    successful_status.exit_code = Some(0);

    for (label, mut edge) in [
        ("cancel_requested", cancel_requested),
        ("timed_out", timed_out),
        ("non_255_exit", non_255_exit),
        ("successful_status", successful_status),
    ] {
        let before_status = edge.status.clone();
        let before_cleanup_status = edge.remote_process_scope.remote_cleanup_status.clone();
        let detected = mark_shell_job_remote_transport_lost_if_detected(
            &mut edge,
            &paths,
            "regression_terminal_readback",
        )
        .unwrap_or_else(|error| panic!("{label} transport detection should read stderr: {error}"));
        println!(
            "readback=act_run_shell_remote_cleanup issue=1277 edge={label} before=status:{before_status} cleanup:{before_cleanup_status} after=status:{} cleanup:{} detected:{detected}",
            edge.status, edge.remote_process_scope.remote_cleanup_status
        );
        assert!(
            !detected,
            "{label} must not be classified as transport loss"
        );
        assert_eq!(edge.status, before_status);
        assert_eq!(
            edge.remote_process_scope.remote_cleanup_status,
            before_cleanup_status
        );
    }

    let no_transport_stderr = "SYNAPSE_REMOTE_PROCESS_V1 job_id=issue1277-guard pid=3487519 pgid=3487519 sid=3487519\nexit 255 without an SSH transport-loss string\n";
    std::fs::write(&paths.stderr_path, no_transport_stderr)
        .unwrap_or_else(|error| panic!("write non-transport stderr log: {error}"));
    let mut no_transport = base.clone();
    let detected = mark_shell_job_remote_transport_lost_if_detected(
        &mut no_transport,
        &paths,
        "regression_terminal_readback",
    )
    .unwrap_or_else(|error| panic!("non-transport detection should read stderr: {error}"));
    println!(
        "readback=act_run_shell_remote_cleanup issue=1277 edge=no_transport_pattern before=exit_nonzero/255 after=status:{} cleanup:{} detected:{detected}",
        no_transport.status, no_transport.remote_process_scope.remote_cleanup_status
    );
    assert!(!detected);
    assert_eq!(no_transport.status, "exit_nonzero");
    assert_eq!(
        no_transport.remote_process_scope.remote_cleanup_status,
        SHELL_REMOTE_CLEANUP_TRACKED
    );
}

#[test]
fn issue1274_shell_status_marks_remote_already_gone_local_transport_stale() {
    let temp = tempfile::TempDir::new()
        .unwrap_or_else(|error| panic!("create temp shell status dir: {error}"));
    let paths = temp_shell_job_paths(&temp);
    let mut status = issue1277_ssh_status("issue1274-stale", "running", &paths);
    status.pid = Some(4242);
    let metadata = parse_remote_process_metadata(
        "SYNAPSE_REMOTE_PROCESS_V1 job_id=issue1274-stale pid=2266815 pgid=2266815 sid=2266815\n",
        "issue1274-stale",
    )
    .unwrap_or_else(|| panic!("remote marker should parse"));
    apply_remote_process_metadata(&mut status, metadata);

    mark_shell_job_remote_already_gone_local_stale(
        &mut status,
        "regression_status_remote_liveness",
        "terminated",
        &[],
        None,
    );

    println!(
        "readback=act_run_shell_status issue=1274 edge=remote_already_gone before=status:running remote:tracked after=status:{} cleanup:{} verified:{} required:{}",
        status.status,
        status.remote_process_scope.remote_cleanup_status,
        status.remote_process_scope.remote_cleanup_verified,
        status.remote_process_scope.remote_cleanup_required
    );
    assert_eq!(status.status, SHELL_JOB_STATUS_REMOTE_EXITED_LOCAL_STALE);
    assert!(shell_job_terminal_status(&status.status));
    assert_eq!(status.exit_code, None);
    assert!(status.completed_at.is_some());
    assert!(status.duration_ms.is_some());
    assert!(!status.remote_process_scope.remote_cleanup_required);
    assert!(status.remote_process_scope.remote_cleanup_verified);
    assert_eq!(
        status.remote_process_scope.remote_cleanup_status,
        SHELL_REMOTE_CLEANUP_ALREADY_GONE
    );
    assert_eq!(status.remote_process_scope.remote_cleanup_error_code, None);
    assert_eq!(status.error_code, None);
    assert!(
        status
            .remote_process_scope
            .remote_cleanup_message
            .as_deref()
            .is_some_and(|message| message.contains("already gone"))
    );
    assert!(
        status
            .remote_process_scope
            .detection_evidence
            .iter()
            .any(|evidence| evidence == "remote_process_already_gone_before_cancel")
    );
    assert!(
        status
            .remote_process_scope
            .detection_evidence
            .iter()
            .any(|evidence| evidence == "local_transport_stale_termination:terminated")
    );
}

#[test]
fn issue1274_remote_liveness_marker_parser_distinguishes_alive_and_gone() {
    let alive = format!("{SHELL_REMOTE_LIVENESS_MARKER} pid=2266815 pgid=2266815 status=alive\n");
    let gone =
        format!("{SHELL_REMOTE_LIVENESS_MARKER} pid=2266815 pgid=2266815 status=already_gone\n");
    let wrong_pid =
        format!("{SHELL_REMOTE_LIVENESS_MARKER} pid=1 pgid=2266815 status=already_gone\n");
    let command = ssh_remote_liveness_command("2266815", "2266815");

    println!(
        "readback=act_run_shell_status issue=1274 edge=liveness_parse alive={:?} gone={:?} command={command:?}",
        parse_remote_liveness_status(&alive, "2266815", "2266815"),
        parse_remote_liveness_status(&gone, "2266815", "2266815")
    );
    assert_eq!(
        parse_remote_liveness_status(&alive, "2266815", "2266815").as_deref(),
        Some("alive")
    );
    assert_eq!(
        parse_remote_liveness_status(&gone, "2266815", "2266815").as_deref(),
        Some("already_gone")
    );
    assert_eq!(
        parse_remote_liveness_status(&wrong_pid, "2266815", "2266815"),
        None
    );
    assert!(command.contains(SHELL_REMOTE_LIVENESS_MARKER));
    assert!(command.contains("ps -o pgid="));
    assert!(!command.contains("kill -TERM"));
    assert!(!command.contains("kill -KILL"));
}

#[test]
fn issue1274_remote_exit_marker_zero_marks_stale_transport_success() {
    let temp = tempfile::TempDir::new()
        .unwrap_or_else(|error| panic!("create temp shell status dir: {error}"));
    let paths = temp_shell_job_paths(&temp);
    let mut status = issue1277_ssh_status("issue1274-exit-zero", "exit_nonzero", &paths);
    status.exit_code = Some(1);
    let stderr = "\
SYNAPSE_REMOTE_PROCESS_V1 job_id=issue1274-exit-zero pid=2266815 pgid=2266815 sid=2266815
SYNAPSE_REMOTE_EXIT_V1 job_id=issue1274-exit-zero pid=2266815 pgid=2266815 exit_code=0
";
    std::fs::write(&paths.stderr_path, stderr)
        .unwrap_or_else(|error| panic!("write remote exit stderr: {error}"));
    refresh_shell_job_remote_metadata_from_outputs(&mut status, &paths)
        .unwrap_or_else(|error| panic!("remote process marker should read: {error}"));

    let reconciled = reconcile_shell_job_remote_exit_marker(
        &mut status,
        &paths,
        false,
        "regression_remote_exit_marker",
    )
    .unwrap_or_else(|error| panic!("remote exit marker should read: {error}"));

    println!(
        "readback=act_run_shell_status issue=1274 edge=remote_exit_zero before=local_exit_nonzero after=status:{} exit_code:{:?} cleanup:{} reconciled:{reconciled}",
        status.status, status.exit_code, status.remote_process_scope.remote_cleanup_status
    );
    assert!(reconciled);
    assert_eq!(status.status, SHELL_JOB_STATUS_REMOTE_EXITED_LOCAL_STALE);
    assert_eq!(status.exit_code, Some(0));
    assert!(status.remote_process_scope.remote_cleanup_verified);
    assert_eq!(
        status.remote_process_scope.remote_cleanup_status,
        SHELL_REMOTE_CLEANUP_ALREADY_GONE
    );
    assert!(
        status
            .remote_process_scope
            .detection_evidence
            .iter()
            .any(|evidence| evidence
                == "remote_exit_marker:SYNAPSE_REMOTE_EXIT_V1:pid=2266815:pgid=2266815:exit_code=0")
    );
}

#[test]
fn issue1274_remote_exit_marker_nonzero_does_not_hide_remote_failure() {
    let temp = tempfile::TempDir::new()
        .unwrap_or_else(|error| panic!("create temp shell status dir: {error}"));
    let paths = temp_shell_job_paths(&temp);
    let mut status = issue1277_ssh_status("issue1274-exit-nonzero", "exit_nonzero", &paths);
    status.exit_code = Some(7);
    let stderr = "\
SYNAPSE_REMOTE_PROCESS_V1 job_id=issue1274-exit-nonzero pid=2266815 pgid=2266815 sid=2266815
SYNAPSE_REMOTE_EXIT_V1 job_id=issue1274-exit-nonzero pid=2266815 pgid=2266815 exit_code=7
";
    std::fs::write(&paths.stderr_path, stderr)
        .unwrap_or_else(|error| panic!("write remote exit stderr: {error}"));
    refresh_shell_job_remote_metadata_from_outputs(&mut status, &paths)
        .unwrap_or_else(|error| panic!("remote process marker should read: {error}"));

    let reconciled = reconcile_shell_job_remote_exit_marker(
        &mut status,
        &paths,
        false,
        "regression_remote_exit_marker",
    )
    .unwrap_or_else(|error| panic!("remote exit marker should read: {error}"));

    println!(
        "readback=act_run_shell_status issue=1274 edge=remote_exit_nonzero before=exit_nonzero/7 after=status:{} exit_code:{:?} cleanup:{} reconciled:{reconciled}",
        status.status, status.exit_code, status.remote_process_scope.remote_cleanup_status
    );
    assert!(!reconciled);
    assert_eq!(status.status, "exit_nonzero");
    assert_eq!(status.exit_code, Some(7));
    assert_eq!(
        status.remote_process_scope.remote_cleanup_status,
        SHELL_REMOTE_CLEANUP_TRACKED
    );
    assert!(
        status
            .remote_process_scope
            .detection_evidence
            .iter()
            .any(|evidence| evidence
                == "remote_exit_marker:SYNAPSE_REMOTE_EXIT_V1:pid=2266815:pgid=2266815:exit_code=7")
    );
}

// #1604: a durable SSH job whose remote process exited 0 in a fraction of a
// second (SYNAPSE_REMOTE_EXIT_V1 emitted) but whose LOCAL ssh.exe wrapper outran
// durable_timeout_ms and was force-terminated must NOT be reported as timed_out.
// Exit-evidence wins: the captured remote exit is the verdict, the local budget
// overrun is downgraded to a warning, and the ACTION_BUDGET_EXPIRED error clears.
#[test]
fn issue1604_local_timeout_does_not_shadow_remote_exit_zero() {
    let temp = tempfile::TempDir::new()
        .unwrap_or_else(|error| panic!("create temp shell status dir: {error}"));
    let paths = temp_shell_job_paths(&temp);
    let mut status = issue1277_ssh_status("issue1604-exit-zero", "timed_out", &paths);
    // Simulate the durable timeout verdict produced by `wait_shell_job_child`
    // when the local ssh wrapper blew its budget while the remote was already gone.
    status.timeout_ms = Some(60_000);
    status.timed_out = true;
    status.exit_code = None;
    status.error_code = Some(error_codes::ACTION_BUDGET_EXPIRED.to_owned());
    status.error_message = Some("durable job timeout_ms cap expired".to_owned());
    let stderr = "\
SYNAPSE_REMOTE_PROCESS_V1 job_id=issue1604-exit-zero pid=1005689 pgid=1005689 sid=1005689
SYNAPSE_REMOTE_EXIT_V1 job_id=issue1604-exit-zero pid=1005689 pgid=1005689 exit_code=0
";
    std::fs::write(&paths.stderr_path, stderr)
        .unwrap_or_else(|error| panic!("write remote exit stderr: {error}"));
    refresh_shell_job_remote_metadata_from_outputs(&mut status, &paths)
        .unwrap_or_else(|error| panic!("remote process marker should read: {error}"));

    println!(
        "readback=act_run_shell_status issue=1604 edge=timeout_over_exit_zero BEFORE status:{} exit_code:{:?} timed_out:{} error_code:{:?} cleanup:{}",
        status.status,
        status.exit_code,
        status.timed_out,
        status.error_code,
        status.remote_process_scope.remote_cleanup_status
    );

    let reconciled =
        reconcile_shell_job_remote_exit_marker(&mut status, &paths, false, "regression_issue1604")
            .unwrap_or_else(|error| panic!("remote exit marker should read: {error}"));

    println!(
        "readback=act_run_shell_status issue=1604 edge=timeout_over_exit_zero AFTER status:{} exit_code:{:?} timed_out:{} error_code:{:?} cleanup:{} reconciled:{reconciled}",
        status.status,
        status.exit_code,
        status.timed_out,
        status.error_code,
        status.remote_process_scope.remote_cleanup_status
    );

    assert!(
        reconciled,
        "the exit marker must override the local timeout"
    );
    assert_eq!(status.status, SHELL_JOB_STATUS_REMOTE_EXITED_LOCAL_STALE);
    assert_eq!(status.exit_code, Some(0));
    assert!(
        !status.timed_out,
        "a captured remote exit must clear the stale local timeout verdict"
    );
    assert_ne!(
        status.error_code.as_deref(),
        Some(error_codes::ACTION_BUDGET_EXPIRED),
        "ACTION_BUDGET_EXPIRED must not survive a captured remote exit"
    );
    assert!(status.remote_process_scope.remote_cleanup_verified);
    assert_eq!(
        status.remote_process_scope.remote_cleanup_status,
        SHELL_REMOTE_CLEANUP_ALREADY_GONE
    );
    assert!(
        status
            .remote_process_scope
            .detection_evidence
            .iter()
            .any(|evidence| evidence.starts_with("local_timeout_overridden_by_remote_exit_marker:")),
        "the downgraded local budget overrun must be preserved as structured warning evidence: {:?}",
        status.remote_process_scope.detection_evidence
    );
}

// #1604 edge: exit code nonzero + a stale local timeout. Exit-evidence is still
// more truthful than "timed_out" — the verdict becomes the real remote failure
// code, never a budget timeout, and the failure is not hidden.
#[test]
fn issue1604_local_timeout_does_not_shadow_remote_exit_nonzero() {
    let temp = tempfile::TempDir::new()
        .unwrap_or_else(|error| panic!("create temp shell status dir: {error}"));
    let paths = temp_shell_job_paths(&temp);
    let mut status = issue1277_ssh_status("issue1604-exit-nonzero", "timed_out", &paths);
    status.timeout_ms = Some(60_000);
    status.timed_out = true;
    status.exit_code = None;
    status.error_code = Some(error_codes::ACTION_BUDGET_EXPIRED.to_owned());
    let stderr = "\
SYNAPSE_REMOTE_PROCESS_V1 job_id=issue1604-exit-nonzero pid=1005689 pgid=1005689 sid=1005689
SYNAPSE_REMOTE_EXIT_V1 job_id=issue1604-exit-nonzero pid=1005689 pgid=1005689 exit_code=7
";
    std::fs::write(&paths.stderr_path, stderr)
        .unwrap_or_else(|error| panic!("write remote exit stderr: {error}"));
    refresh_shell_job_remote_metadata_from_outputs(&mut status, &paths)
        .unwrap_or_else(|error| panic!("remote process marker should read: {error}"));

    let reconciled =
        reconcile_shell_job_remote_exit_marker(&mut status, &paths, false, "regression_issue1604")
            .unwrap_or_else(|error| panic!("remote exit marker should read: {error}"));

    println!(
        "readback=act_run_shell_status issue=1604 edge=timeout_over_exit_nonzero AFTER status:{} exit_code:{:?} timed_out:{} error_code:{:?} reconciled:{reconciled}",
        status.status, status.exit_code, status.timed_out, status.error_code
    );

    assert!(reconciled);
    assert_eq!(status.exit_code, Some(7));
    assert!(!status.timed_out);
    assert_ne!(
        status.error_code.as_deref(),
        Some(error_codes::ACTION_BUDGET_EXPIRED)
    );
}

// #1604 edge: the local wrapper timed out but the connection dropped before any
// SYNAPSE_REMOTE_EXIT_V1 marker was captured. With no exit-evidence, the loud
// timed_out verdict MUST stand — we never invent a success from an absent marker.
#[test]
fn issue1604_absent_remote_exit_marker_keeps_timeout_verdict() {
    let temp = tempfile::TempDir::new()
        .unwrap_or_else(|error| panic!("create temp shell status dir: {error}"));
    let paths = temp_shell_job_paths(&temp);
    let mut status = issue1277_ssh_status("issue1604-no-marker", "timed_out", &paths);
    status.timeout_ms = Some(60_000);
    status.timed_out = true;
    status.exit_code = None;
    status.error_code = Some(error_codes::ACTION_BUDGET_EXPIRED.to_owned());
    // Process marker present (tracked) but NO exit marker: transport dropped
    // before the remote could report its exit.
    let stderr = "\
SYNAPSE_REMOTE_PROCESS_V1 job_id=issue1604-no-marker pid=1005689 pgid=1005689 sid=1005689
client_loop: send disconnect: Broken pipe
";
    std::fs::write(&paths.stderr_path, stderr)
        .unwrap_or_else(|error| panic!("write stderr: {error}"));
    refresh_shell_job_remote_metadata_from_outputs(&mut status, &paths)
        .unwrap_or_else(|error| panic!("remote process marker should read: {error}"));

    let reconciled =
        reconcile_shell_job_remote_exit_marker(&mut status, &paths, false, "regression_issue1604")
            .unwrap_or_else(|error| panic!("reconcile should not error: {error}"));

    println!(
        "readback=act_run_shell_status issue=1604 edge=no_marker AFTER status:{} timed_out:{} reconciled:{reconciled}",
        status.status, status.timed_out
    );

    assert!(!reconciled, "no exit marker => no override");
    assert!(
        status.timed_out,
        "the timeout verdict must stand without exit-evidence"
    );
    assert_eq!(
        status.error_code.as_deref(),
        Some(error_codes::ACTION_BUDGET_EXPIRED)
    );
}

// #1604 inline parity (#1588): a fast local command must report its real exit
// code promptly and never be flagged timed_out, even under a large timeout_ms.
#[cfg(windows)]
#[tokio::test]
async fn issue1604_inline_fast_command_reports_exit_promptly() {
    let mut zero = TokioCommand::new("cmd.exe")
        .args(["/c", "exit 0"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap_or_else(|error| panic!("spawn zero-exit child: {error}"));
    let (zero_exit, zero_timed_out) = wait_shell_child(&mut zero, 60_000)
        .await
        .unwrap_or_else(|error| panic!("wait zero-exit: {error:?}"));
    println!(
        "readback=wait_shell_child issue=1604 edge=fast_exit_zero after=exit_code:{zero_exit:?} timed_out:{zero_timed_out}"
    );
    assert_eq!(zero_exit, Some(0));
    assert!(!zero_timed_out, "a fast clean exit is never timed_out");

    // Zero-duration nonzero exit: exit-evidence (code 3) preserved, not timed_out.
    let mut nonzero = TokioCommand::new("cmd.exe")
        .args(["/c", "exit 3"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap_or_else(|error| panic!("spawn nonzero-exit child: {error}"));
    let (nonzero_exit, nonzero_timed_out) = wait_shell_child(&mut nonzero, 60_000)
        .await
        .unwrap_or_else(|error| panic!("wait nonzero-exit: {error:?}"));
    println!(
        "readback=wait_shell_child issue=1604 edge=fast_exit_nonzero after=exit_code:{nonzero_exit:?} timed_out:{nonzero_timed_out}"
    );
    assert_eq!(nonzero_exit, Some(3));
    assert!(!nonzero_timed_out);
}

// #1604 inline: timeout_ms must bound elapsed time. A real 5 s process under a
// 200 ms cap must return timed_out well before its natural exit.
#[cfg(windows)]
#[tokio::test]
async fn issue1604_inline_timeout_bounds_elapsed_time() {
    let started = Instant::now();
    let mut child = TokioCommand::new("powershell.exe")
        .args(["-NoProfile", "-Command", "Start-Sleep -Seconds 5"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap_or_else(|error| panic!("spawn sleeper child: {error}"));
    let (exit_code, timed_out) = wait_shell_child(&mut child, 200)
        .await
        .unwrap_or_else(|error| panic!("wait sleeper: {error:?}"));
    let elapsed = started.elapsed();
    println!(
        "readback=wait_shell_child issue=1604 edge=timeout_bounds after=exit_code:{exit_code:?} timed_out:{timed_out} elapsed_ms:{}",
        elapsed.as_millis()
    );
    assert!(
        timed_out,
        "a 5 s process under a 200 ms cap must be timed_out"
    );
    assert!(
        elapsed < Duration::from_secs(4),
        "timeout_ms must bound elapsed time; the 5 s process ran {} ms",
        elapsed.as_millis()
    );
}

#[test]
fn issue1283_bash_login_errexit_exit_one_surfaces_specific_hint() {
    let temp = tempfile::TempDir::new()
        .unwrap_or_else(|error| panic!("create temp shell status dir: {error}"));
    let paths = temp_shell_job_paths(&temp);
    let mut status = issue1277_ssh_status("issue1283-bash-login", "exit_nonzero", &paths);
    status.exit_code = Some(1);
    status.args = vec![
        "-l".to_owned(),
        "croyse".to_owned(),
        "aiwonder.mst.com".to_owned(),
        "bash -lc 'set +e; true; EC=$?; set -e; printf \"inner_exit=%s\\n\" \"$EC\"; exit \"$EC\"'"
            .to_owned(),
    ];
    status.remote_process_scope =
        ssh_remote_process_scope("ssh.exe", &status.args, "regression_issue1283");

    let diagnostics = shell_job_status_diagnostics(&status, false, 23, 211);

    println!(
        "readback=act_run_shell_status issue=1283 edge=bash_login_errexit hints={:?}",
        diagnostics.actionable_hints
    );
    assert!(diagnostics.actionable_hints.iter().any(|hint| hint
        == "bash_login_shell_errexit_can_override_inner_exit_status_use_bash_c_or_disable_errexit_before_exit"));
}

#[test]
fn issue1283_non_login_bash_errexit_exit_one_keeps_generic_ssh_hints() {
    let temp = tempfile::TempDir::new()
        .unwrap_or_else(|error| panic!("create temp shell status dir: {error}"));
    let paths = temp_shell_job_paths(&temp);
    let mut status = issue1277_ssh_status("issue1283-bash-non-login", "exit_nonzero", &paths);
    status.exit_code = Some(1);
    status.args = vec![
        "-l".to_owned(),
        "croyse".to_owned(),
        "aiwonder.mst.com".to_owned(),
        "bash -c 'set +e; true; EC=$?; set -e; printf \"inner_exit=%s\\n\" \"$EC\"; exit \"$EC\"'"
            .to_owned(),
    ];
    status.remote_process_scope =
        ssh_remote_process_scope("ssh.exe", &status.args, "regression_issue1283");

    let diagnostics = shell_job_status_diagnostics(&status, false, 23, 211);

    println!(
        "readback=act_run_shell_status issue=1283 edge=non_login_bash hints={:?}",
        diagnostics.actionable_hints
    );
    assert!(!diagnostics.actionable_hints.iter().any(|hint| hint
        == "bash_login_shell_errexit_can_override_inner_exit_status_use_bash_c_or_disable_errexit_before_exit"));
    assert!(
        diagnostics
            .actionable_hints
            .iter()
            .any(|hint| hint == "check_remote_command_tty_stdin_and_auth_prompts")
    );
}

#[test]
fn ssh_cleanup_command_parts_prefers_live_original_args_over_safe_status_args() {
    let temp = tempfile::TempDir::new()
        .unwrap_or_else(|error| panic!("create temp shell status dir: {error}"));
    let paths = ShellJobPaths {
        job_dir: temp.path().to_path_buf(),
        stdout_path: temp.path().join("stdout.log"),
        stderr_path: temp.path().join("stderr.log"),
        status_path: temp.path().join("status.json"),
        request_path: temp.path().join("request.json"),
        remote_cleanup_path: temp.path().join("remote-cleanup.json"),
    };
    let safe_args = vec![
        "-i".to_owned(),
        "[redacted-arg:sha256=deadbeef:bytes=48]".to_owned(),
        "-l".to_owned(),
        "croyse".to_owned(),
        "aiwonder.mst.com".to_owned(),
        "true".to_owned(),
    ];
    let original_args = vec![
        "-i".to_owned(),
        "//wsl.localhost/Ubuntu-24.04/home/cabdru/.ssh/id_ed25519".to_owned(),
        "-l".to_owned(),
        "croyse".to_owned(),
        "aiwonder.mst.com".to_owned(),
        "true".to_owned(),
    ];
    let params = ActRunShellStartParams {
        command: "ssh".to_owned(),
        args: safe_args,
        working_dir: None,
        env: BTreeMap::new(),
        timeout_ms: None,
        job_id: Some("issue972-redacted-status".to_owned()),
    };
    let authorization = RunShellAuthorization {
        command_line:
            "ssh -i [redacted-arg:sha256=deadbeef:bytes=48] -l croyse aiwonder.mst.com true"
                .to_owned(),
        matched_pattern: "^ssh".to_owned(),
    };
    let status = shell_job_status_record(
        "issue972-redacted-status",
        "ok",
        &params,
        &paths,
        "request-sha",
        &authorization,
        "2026-06-14T00:00:00Z".to_owned(),
        Some(1234),
        None,
    );

    let live_invocation = shell_job_cleanup_invocation(&status, Some(&original_args), None)
        .unwrap_or_else(|| panic!("parse live original cleanup invocation"));
    let live_parts = ssh_direct_command_parts(&live_invocation.args)
        .unwrap_or_else(|| panic!("parse live original cleanup args"));
    assert!(
        live_parts
            .control_args
            .iter()
            .any(|arg| arg == "//wsl.localhost/Ubuntu-24.04/home/cabdru/.ssh/id_ed25519")
    );
    assert!(
        !live_parts
            .control_args
            .iter()
            .any(|arg| arg.contains("[redacted-arg:"))
    );

    let persisted_invocation = shell_job_cleanup_invocation(&status, None, None)
        .unwrap_or_else(|| panic!("parse persisted cleanup invocation"));
    let persisted_parts = ssh_direct_command_parts(&persisted_invocation.args)
        .unwrap_or_else(|| panic!("parse persisted cleanup args"));
    assert!(
        persisted_parts
            .control_args
            .iter()
            .any(|arg| arg.contains("[redacted-arg:"))
    );
}

#[test]
fn shell_wrapped_ssh_cleanup_sidecar_survives_redacted_status_args() {
    let temp = tempfile::TempDir::new()
        .unwrap_or_else(|error| panic!("create temp shell status dir: {error}"));
    let paths = ShellJobPaths {
        job_dir: temp.path().to_path_buf(),
        stdout_path: temp.path().join("stdout.log"),
        stderr_path: temp.path().join("stderr.log"),
        status_path: temp.path().join("status.json"),
        request_path: temp.path().join("request.json"),
        remote_cleanup_path: temp.path().join("remote-cleanup.json"),
    };
    let remote_body = format!("bash -lc 'exec -a issue1019 {}'", "sleep 600 ".repeat(80));
    let script = format!(
        "ssh -o BatchMode=yes -i //wsl.localhost/Ubuntu-24.04/home/cabdru/.ssh/id_ed25519 -l croyse aiwonder.mst.com \"{remote_body}\""
    );
    let params = ActRunShellStartParams {
        command: "powershell.exe".to_owned(),
        args: vec![
            "-NoLogo".to_owned(),
            "-NoProfile".to_owned(),
            "-NonInteractive".to_owned(),
            "-Command".to_owned(),
            script,
        ],
        working_dir: None,
        env: BTreeMap::new(),
        timeout_ms: None,
        job_id: Some("issue1019-sidecar".to_owned()),
    };
    let authorization = RunShellAuthorization {
        command_line: shell_command_line_from_parts(&params.command, &params.args),
        matched_pattern: "__any_permitted__".to_owned(),
    };
    let status = shell_job_status_record(
        "issue1019-sidecar",
        "running",
        &params,
        &paths,
        "request-sha",
        &authorization,
        "2026-06-15T00:00:00Z".to_owned(),
        Some(1234),
        None,
    );

    write_shell_remote_cleanup_invocation(&paths, &params)
        .unwrap_or_else(|error| panic!("cleanup sidecar should write: {error}"));
    let cleanup = read_shell_remote_cleanup_invocation(&paths, "issue1019-sidecar")
        .unwrap_or_else(|error| panic!("cleanup sidecar should read: {error}"))
        .unwrap_or_else(|| panic!("cleanup sidecar should exist"));
    let persisted_invocation = shell_job_cleanup_invocation(&status, None, Some(&cleanup))
        .unwrap_or_else(|| panic!("parse persisted cleanup sidecar invocation"));
    let persisted_parts = ssh_direct_command_parts(&persisted_invocation.args)
        .unwrap_or_else(|| panic!("parse persisted sidecar cleanup args"));

    println!(
        "readback=act_run_shell_remote_cleanup edge=shell_wrapped_redacted_status before=args_redacted:{} after=cleanup:{cleanup:?} invocation:{persisted_invocation:?}",
        status.args_redacted
    );
    assert_eq!(persisted_invocation.command, "ssh");
    assert!(status.args_redacted);
    assert!(
        persisted_parts
            .control_args
            .iter()
            .any(|arg| arg == "//wsl.localhost/Ubuntu-24.04/home/cabdru/.ssh/id_ed25519")
    );
    assert!(
        !persisted_parts
            .control_args
            .iter()
            .any(|arg| arg.contains("[redacted-arg:"))
    );
    assert!(
        !cleanup
            .control_args
            .iter()
            .any(|arg| arg.contains("exec -a issue1019"))
    );
    assert_eq!(cleanup.remote_identity, "aiwonder.mst.com");
    assert_eq!(cleanup.source_evidence, "shell_wrapped_ssh:powershell");
}

#[test]
fn shell_cleanup_output_excerpt_is_bounded_and_one_line() {
    let input = format!("line1\r\n{}", "x".repeat(600));
    let excerpt = shell_cleanup_output_excerpt(&input);

    assert!(excerpt.contains("\\r\\n"));
    assert!(!excerpt.contains('\r'));
    assert!(!excerpt.contains('\n'));
    assert!(excerpt.ends_with("...[truncated]"));
    assert!(excerpt.len() <= 530);
}

#[test]
fn shell_remote_process_marker_updates_cleanup_handle() {
    let mut scope = ssh_remote_process_scope(
        "ssh.exe",
        &["aiwonder".to_owned(), "sleep 60".to_owned()],
        "regression_marker",
    );
    let mut status = ActRunShellJobStatus {
        schema_version: 4,
        job_id: "issue940-marker".to_owned(),
        session_id: None,
        status: "running".to_owned(),
        pid: Some(4242),
        command: "ssh.exe".to_owned(),
        command_metadata_policy: SHELL_COMMAND_METADATA_POLICY.to_owned(),
        args: vec!["aiwonder".to_owned(), "sleep 60".to_owned()],
        command_line: "ssh.exe aiwonder \"sleep 60\"".to_owned(),
        args_redacted: false,
        command_line_redacted: false,
        args_original_count: 2,
        args_original_bytes: 17,
        args_sha256: "args-sha".to_owned(),
        command_line_original_bytes: 27,
        command_line_sha256: "command-sha".to_owned(),
        working_dir: None,
        session_dir: None,
        effective_working_dir: None,
        env_keys: Vec::new(),
        session_env_keys: Vec::new(),
        timeout_ms: None,
        started_at: "2026-06-13T00:00:00Z".to_owned(),
        completed_at: None,
        duration_ms: None,
        exit_code: None,
        timed_out: false,
        cancel_requested: false,
        error_code: None,
        error_message: None,
        stdout_path: "stdout.log".to_owned(),
        stderr_path: "stderr.log".to_owned(),
        status_path: "status.json".to_owned(),
        request_sha256: "request-sha".to_owned(),
        matched_pattern: "^ssh".to_owned(),
        remote_process_scope: scope.clone(),
        diagnostics: None,
    };
    let stderr =
        "noise\nSYNAPSE_REMOTE_PROCESS_V1 job_id=issue940-marker pid=12345 pgid=12345 sid=12345\n";
    let metadata =
        parse_remote_process_metadata(stderr, "issue940-marker").expect("marker line should parse");

    apply_remote_process_metadata(&mut status, metadata);
    scope = status.remote_process_scope.clone();

    println!(
        "readback=act_run_shell_remote_tracking edge=marker_parse before={stderr:?} after={scope:?}"
    );
    assert_eq!(scope.remote_cleanup_status, SHELL_REMOTE_CLEANUP_TRACKED);
    assert_eq!(scope.remote_process_id.as_deref(), Some("12345"));
    assert_eq!(scope.remote_process_group_id.as_deref(), Some("12345"));
    assert!(!scope.remote_cleanup_verified);
    assert!(
        scope
            .detection_evidence
            .iter()
            .any(|evidence| evidence.contains("remote_session_id:12345"))
    );

    let concatenated_stderr = "user-stderr-without-newlineSYNAPSE_REMOTE_PROCESS_V1 job_id=issue940-marker pid=54321 pgid=54321 sid=54321\n";
    let concatenated_metadata =
        parse_remote_process_metadata(concatenated_stderr, "issue940-marker")
            .expect("marker concatenated after user stderr should parse");
    println!(
        "readback=act_run_shell_remote_tracking edge=marker_after_user_stderr before={concatenated_stderr:?} after={concatenated_metadata:?}"
    );
    assert_eq!(concatenated_metadata.pid, "54321");
    assert_eq!(concatenated_metadata.pgid, "54321");
}

#[test]
fn shell_remote_cleanup_command_uses_dash_compatible_negative_pgid() {
    let command = ssh_remote_cleanup_command("12345", "12345");

    println!("readback=act_run_shell_remote_cleanup edge=dash_kill_syntax after={command:?}");
    assert!(command.contains("kill -TERM -\"$pgid\""));
    assert!(command.contains("kill -KILL -\"$pgid\""));
    assert!(!command.contains("kill -TERM --"));
    assert!(!command.contains("kill -KILL --"));
}

#[test]
fn shell_status_diagnostics_classifies_scp_default_sftp_no_output() {
    let temp = tempfile::TempDir::new()
        .unwrap_or_else(|error| panic!("create temp shell status dir: {error}"));
    let paths = ShellJobPaths {
        job_dir: temp.path().to_path_buf(),
        stdout_path: temp.path().join("stdout.log"),
        stderr_path: temp.path().join("stderr.log"),
        status_path: temp.path().join("status.json"),
        request_path: temp.path().join("request.json"),
        remote_cleanup_path: temp.path().join("remote-cleanup.json"),
    };
    let params = ActRunShellStartParams {
        command: "scp.exe".to_owned(),
        args: vec![
            "local.txt".to_owned(),
            "aiwonder:/tmp/synapse885-local.txt".to_owned(),
        ],
        working_dir: None,
        env: BTreeMap::new(),
        timeout_ms: None,
        job_id: Some("issue885-scp-diagnostics".to_owned()),
    };
    let authorization = RunShellAuthorization {
        command_line: "scp.exe local.txt aiwonder:/tmp/synapse885-local.txt".to_owned(),
        matched_pattern: "^scp".to_owned(),
    };
    let status = shell_job_status_record(
        "issue885-scp-diagnostics",
        "running",
        &params,
        &paths,
        "request-sha",
        &authorization,
        "2026-06-12T00:00:00Z".to_owned(),
        Some(1234),
        None,
    );

    let diagnostics = shell_job_status_diagnostics(&status, true, 0, 0);
    let transfer = diagnostics
        .transfer
        .as_ref()
        .expect("scp diagnostics should identify transfer family");

    println!(
        "readback=act_run_shell_status edge=scp_default_sftp_no_output before=stdout:0,stderr:0,args:{:?} after={diagnostics:?}",
        params.args
    );
    assert_eq!(diagnostics.output_state, "running_no_output");
    assert_eq!(transfer.client, "scp");
    assert_eq!(transfer.protocol_hint, "scp_default_sftp_protocol");
    assert_eq!(transfer.remote_identity.as_deref(), Some("aiwonder"));
    assert!(
        diagnostics
            .actionable_hints
            .iter()
            .any(|hint| hint.contains("retry_scp_with_-O"))
    );
    assert!(
        diagnostics
            .actionable_hints
            .iter()
            .any(|hint| hint.contains("rerun_with_-v"))
    );
}

#[test]
fn shell_status_diagnostics_classifies_scp_legacy_o_flag() {
    let args = vec![
        "-O".to_owned(),
        "local.txt".to_owned(),
        "aiwonder:/tmp/synapse885-local.txt".to_owned(),
    ];

    let protocol_hint = shell_transfer_protocol_hint("scp", &args);
    let remote_identity = scp_remote_identity(&args);

    println!(
        "readback=act_run_shell_status edge=scp_legacy_flag before=args:{args:?} after=protocol:{protocol_hint} remote:{remote_identity:?}"
    );
    assert_eq!(protocol_hint, "scp_legacy_protocol_forced_by_-O");
    assert_eq!(remote_identity.as_deref(), Some("aiwonder"));
}

#[test]
fn shell_remote_scope_normalizes_legacy_direct_ssh_status_file() {
    let temp = tempfile::TempDir::new()
        .unwrap_or_else(|error| panic!("create temp shell status dir: {error}"));
    let paths = ShellJobPaths {
        job_dir: temp.path().to_path_buf(),
        stdout_path: temp.path().join("stdout.log"),
        stderr_path: temp.path().join("stderr.log"),
        status_path: temp.path().join("status.json"),
        request_path: temp.path().join("request.json"),
        remote_cleanup_path: temp.path().join("remote-cleanup.json"),
    };
    let params = ActRunShellStartParams {
        command: "ssh.exe".to_owned(),
        args: vec!["aiwonder".to_owned(), "sleep".to_owned(), "60".to_owned()],
        working_dir: None,
        env: BTreeMap::new(),
        timeout_ms: None,
        job_id: Some("issue827-legacy".to_owned()),
    };
    let authorization = RunShellAuthorization {
        command_line: "ssh.exe aiwonder sleep 60".to_owned(),
        matched_pattern: "^ssh".to_owned(),
    };
    let mut status = serde_json::to_value(shell_job_status_record(
        "issue827-legacy",
        "running",
        &params,
        &paths,
        "request-sha",
        &authorization,
        "2026-06-10T00:00:00Z".to_owned(),
        Some(1234),
        None,
    ))
    .unwrap_or_else(|error| panic!("status should encode to JSON: {error}"));
    status["schema_version"] = json!(2);
    status
        .as_object_mut()
        .expect("status is an object")
        .remove("remote_process_scope");
    std::fs::write(
        &paths.status_path,
        serde_json::to_vec_pretty(&status).unwrap(),
    )
    .unwrap_or_else(|error| panic!("write legacy status file: {error}"));

    let read = read_shell_job_status(&paths.status_path, "issue827-legacy")
        .unwrap_or_else(|error| panic!("legacy status should read: {error}"));

    println!(
        "readback=act_run_shell_remote_scope edge=legacy_status before={status} after={:?}",
        read.remote_process_scope
    );
    assert_eq!(
        read.remote_process_scope.transport,
        SHELL_REMOTE_TRANSPORT_SSH
    );
    assert_eq!(
        read.remote_process_scope.remote_identity.as_deref(),
        Some("aiwonder")
    );
}

#[test]
fn shell_remote_scope_marks_cancelled_ssh_cleanup_unverified() {
    let temp = tempfile::TempDir::new()
        .unwrap_or_else(|error| panic!("create temp shell status dir: {error}"));
    let paths = ShellJobPaths {
        job_dir: temp.path().to_path_buf(),
        stdout_path: temp.path().join("stdout.log"),
        stderr_path: temp.path().join("stderr.log"),
        status_path: temp.path().join("status.json"),
        request_path: temp.path().join("request.json"),
        remote_cleanup_path: temp.path().join("remote-cleanup.json"),
    };
    let params = ActRunShellStartParams {
        command: "ssh.exe".to_owned(),
        args: vec!["aiwonder".to_owned(), "sleep".to_owned(), "60".to_owned()],
        working_dir: None,
        env: BTreeMap::new(),
        timeout_ms: None,
        job_id: Some("issue827-cancel".to_owned()),
    };
    let authorization = RunShellAuthorization {
        command_line: "ssh.exe aiwonder sleep 60".to_owned(),
        matched_pattern: "^ssh".to_owned(),
    };
    let mut status = shell_job_status_record(
        "issue827-cancel",
        "cancel_requested",
        &params,
        &paths,
        "request-sha",
        &authorization,
        "2026-06-10T00:00:00Z".to_owned(),
        Some(1234),
        None,
    );

    mark_shell_job_remote_cleanup_unverified(&mut status, "act_run_shell_cancel", "terminated");
    let termination_status =
        remote_aware_termination_status("terminated", &status.remote_process_scope);

    println!(
        "readback=act_run_shell_remote_scope edge=cancel_unverified before=terminated after=status:{termination_status} scope:{:?}",
        status.remote_process_scope
    );
    assert_eq!(
        termination_status,
        "local_ssh_client_terminated_remote_cleanup_unverified"
    );
    assert_eq!(
        status.error_code.as_deref(),
        Some(error_codes::ACTION_REMOTE_PROCESS_CLEANUP_UNVERIFIED)
    );
    assert_eq!(
        status
            .remote_process_scope
            .remote_cleanup_error_code
            .as_deref(),
        Some(error_codes::ACTION_REMOTE_PROCESS_CLEANUP_UNVERIFIED)
    );
    assert!(!status.remote_process_scope.remote_cleanup_verified);
}

#[test]
fn shell_rejects_direct_chromium_remote_debugging_without_popup_safe_flags() {
    let params = shell_params(
        "chrome.exe",
        vec!["--remote-debugging-port=9222", "about:blank"],
        30_000,
    );

    let error = validate_run_shell_params(&params)
        .expect_err("direct unsafe Chrome remote-debugging shell launch must fail closed");

    println!(
        "readback=act_run_shell_chromium_policy edge=direct_unsafe before=args:{:?} after={:?}",
        params.args, error.data
    );
    assert_eq!(
        extract_error_code(&error),
        error_codes::A11Y_CDP_DEBUGGER_WARNING_UNSUPPRESSED
    );
    assert_eq!(
        error
            .data
            .as_ref()
            .and_then(|data| data.get("reason"))
            .and_then(|reason| reason.as_str()),
        Some("chromium_remote_debugging_not_popup_safe")
    );
    assert!(
        error
            .data
            .as_ref()
            .and_then(|data| data.get("required_invariant"))
            .and_then(|value| value.as_str())
            .is_some_and(|value| value.contains("--silent-debugger-extension-api"))
    );
}

#[test]
fn shell_allows_direct_chromium_remote_debugging_with_popup_safe_flags() {
    let safe_profile = cdp_automation_profile_dir();
    let safe_profile_arg = format!("--user-data-dir={}", safe_profile.display());
    let params = shell_params(
        "chrome.exe",
        vec![
            "--remote-debugging-port=0",
            safe_profile_arg.as_str(),
            "--silent-debugger-extension-api",
            "--disable-extensions",
            "about:blank",
        ],
        30_000,
    );

    println!(
        "readback=act_run_shell_chromium_policy edge=direct_safe before=args:{:?}",
        params.args
    );
    validate_run_shell_params(&params)
        .unwrap_or_else(|error| panic!("popup-safe direct Chrome shell launch rejected: {error}"));
}

#[test]
fn shell_rejects_direct_chromium_layout_infobar_flag_even_when_silent() {
    let safe_profile = cdp_automation_profile_dir();
    let safe_profile_arg = format!("--user-data-dir={}", safe_profile.display());
    let params = shell_params(
        "chrome.exe",
        vec![
            "--remote-debugging-pipe",
            safe_profile_arg.as_str(),
            "--silent-debugger-extension-api",
            "--disable-extensions",
            "--disable-blink-features=AutomationControlled",
            "about:blank",
        ],
        30_000,
    );

    let error = validate_run_shell_params(&params)
        .expect_err("layout-shifting Chrome flags must fail closed even with silent debugger");

    println!(
        "readback=act_run_shell_chromium_policy edge=direct_layout_flag before=args:{:?} after={:?}",
        params.args, error.data
    );
    assert_eq!(
        extract_error_code(&error),
        error_codes::A11Y_CDP_DEBUGGER_WARNING_UNSUPPRESSED
    );
    let error_text = format!("{error:?}");
    assert!(error_text.contains("has_layout_shifting_infobar_flags"));
    assert!(error_text.contains("AutomationControlled"));
}

#[test]
fn shell_rejects_wrapped_chromium_layout_infobar_launch() {
    let snippet = r#"& "C:\Program Files\Google\Chrome\Application\chrome.exe" --remote-debugging-pipe --user-data-dir="$env:LOCALAPPDATA\ms-playwright-mcp\mcp-chrome-issue1260" --disable-blink-features=AutomationControlled about:blank"#;
    let params = shell_params(
        "powershell.exe",
        vec!["-NoProfile", "-Command", snippet],
        30_000,
    );

    let error = validate_run_shell_params(&params)
        .expect_err("shell-wrapped layout-shifting Chrome launch must fail closed");

    println!(
        "readback=act_run_shell_chromium_policy edge=wrapped_layout_flag before=args:{:?} after={:?}",
        params.args, error.data
    );
    assert_eq!(
        extract_error_code(&error),
        error_codes::A11Y_CDP_DEBUGGER_WARNING_UNSUPPRESSED
    );
    assert_eq!(
        error
            .data
            .as_ref()
            .and_then(|data| data.get("reason"))
            .and_then(|reason| reason.as_str()),
        Some("shell_wrapped_chromium_remote_debugging_not_popup_safe")
    );
    assert!(
        error
            .data
            .as_ref()
            .and_then(|data| data.get("shell_markers"))
            .and_then(|markers| markers.as_array())
            .is_some_and(|markers| markers
                .iter()
                .any(|marker| marker == "layout_flag_automationcontrolled"))
    );
}

#[test]
fn shell_rejects_known_playwright_mcp_launcher_but_allows_text_search() {
    let launcher = shell_params(
        "cmd.exe",
        vec!["/c", "npx @playwright/mcp --browser chrome"],
        30_000,
    );

    let error = validate_run_shell_params(&launcher)
        .expect_err("known Playwright MCP browser launcher must fail closed");
    println!(
        "readback=act_run_shell_chromium_policy edge=playwright_mcp before=args:{:?} after={:?}",
        launcher.args, error.data
    );
    assert_eq!(
        extract_error_code(&error),
        error_codes::A11Y_CDP_DEBUGGER_WARNING_UNSUPPRESSED
    );

    let search = shell_params("rg", vec!["@playwright/mcp"], 30_000);
    validate_run_shell_params(&search)
        .unwrap_or_else(|error| panic!("text search mentioning Playwright MCP rejected: {error}"));

    let search_remote_debug = shell_params(
        "rg",
        vec!["chrome.exe --remote-debugging-pipe @playwright/mcp"],
        30_000,
    );
    validate_run_shell_params(&search_remote_debug).unwrap_or_else(|error| {
        panic!("text search mentioning remote debugging rejected: {error}")
    });
}

#[test]
fn shell_rejects_known_playwright_mcp_launcher_from_bare_shell_names() {
    for command in ["cmd", "powershell", "pwsh"] {
        let params = shell_params(
            command,
            vec!["/c", "npx @playwright/mcp --browser chrome"],
            30_000,
        );

        let error = validate_run_shell_params(&params)
            .expect_err("bare shell names must not bypass Playwright MCP launch guard");
        println!(
            "readback=act_run_shell_chromium_policy edge=bare_shell command={command} before=args:{:?} after={:?}",
            params.args, error.data
        );
        assert_eq!(
            extract_error_code(&error),
            error_codes::A11Y_CDP_DEBUGGER_WARNING_UNSUPPRESSED
        );
        assert_eq!(
            error
                .data
                .as_ref()
                .and_then(|data| data.get("reason"))
                .and_then(|reason| reason.as_str()),
            Some("known_playwright_mcp_browser_launcher_denied")
        );
    }
}

#[test]
fn shell_allows_read_only_process_query_mentioning_playwright_mcp() {
    let snippet = "$rows = Get-CimInstance Win32_Process | Where-Object { $_.Name -match '^(chrome|node|cmd)\\.exe$' -and (($_.CommandLine -like '*ms-playwright-mcp*') -or ($_.CommandLine -like '*@playwright/mcp*')) }; $rows | ConvertTo-Json";
    let params = shell_params(
        "powershell.exe",
        vec!["-NoProfile", "-Command", snippet],
        30_000,
    );

    println!(
        "readback=act_run_shell_chromium_policy edge=readonly_process_query before=args:{:?}",
        params.args
    );
    validate_run_shell_params(&params).unwrap_or_else(|error| {
        panic!("read-only process query mentioning Playwright MCP rejected: {error}")
    });
}

#[test]
fn shell_allows_read_only_process_query_mentioning_remote_debugging() {
    let snippet = "$rows = Get-CimInstance Win32_Process | Where-Object { $_.Name -eq 'chrome.exe' -and $_.CommandLine -like '*--remote-debugging-port=9222*' } | Select-Object ProcessId,CommandLine; $rows | ConvertTo-Json";
    let params = shell_params(
        "powershell.exe",
        vec!["-NoProfile", "-Command", snippet],
        30_000,
    );

    println!(
        "readback=act_run_shell_chromium_policy edge=readonly_remote_debugging_query before=args:{:?}",
        params.args
    );
    validate_run_shell_params(&params).unwrap_or_else(|error| {
        panic!("read-only process query mentioning remote debugging rejected: {error}")
    });
}

#[test]
fn shell_params_reject_command_string_with_embedded_args() {
    let params = shell_params(
        "powershell -NoProfile -Command Write-Output SYN769",
        Vec::new(),
        30_000,
    );

    let error = match validate_run_shell_params(&params) {
        Ok(()) => panic!("command string with embedded args should be rejected"),
        Err(error) => error,
    };

    let reason = error
        .data
        .as_ref()
        .and_then(|data| data.get("reason"))
        .and_then(|reason| reason.as_str());
    println!(
        "readback=act_run_shell_command_shape edge=embedded_args before={:?} after_reason={reason:?}",
        params.command
    );
    assert_eq!(reason, Some("command_contains_arguments"));
}

#[test]
fn shell_params_reject_quoted_command_path() {
    let params = shell_params(
        r#""C:\Program Files\PowerShell\7\pwsh.exe""#,
        Vec::new(),
        30_000,
    );

    let error = match validate_run_shell_params(&params) {
        Ok(()) => panic!("quoted command path should be rejected"),
        Err(error) => error,
    };

    let reason = error
        .data
        .as_ref()
        .and_then(|data| data.get("reason"))
        .and_then(|reason| reason.as_str());
    println!(
        "readback=act_run_shell_command_shape edge=quoted_path before={:?} after_reason={reason:?}",
        params.command
    );
    assert_eq!(reason, Some("command_must_not_be_quoted"));
}

#[test]
fn shell_params_reject_unbalanced_command_quote() {
    let params = shell_params(
        r#""C:\Program Files\PowerShell\7\pwsh.exe"#,
        Vec::new(),
        30_000,
    );

    let error = match validate_run_shell_params(&params) {
        Ok(()) => panic!("unbalanced command quote should be rejected"),
        Err(error) => error,
    };

    let reason = error
        .data
        .as_ref()
        .and_then(|data| data.get("reason"))
        .and_then(|reason| reason.as_str());
    println!(
        "readback=act_run_shell_command_shape edge=unbalanced_quote before={:?} after_reason={reason:?}",
        params.command
    );
    assert_eq!(reason, Some("command_has_unbalanced_quote"));
}

#[test]
fn shell_params_allow_existing_command_path_with_spaces() {
    let dir = tempfile::TempDir::new()
        .unwrap_or_else(|error| panic!("create temp shell path dir: {error}"));
    let nested = dir.path().join("dir with spaces");
    std::fs::create_dir_all(&nested)
        .unwrap_or_else(|error| panic!("create nested temp path: {error}"));
    let command_path = nested.join("tool with spaces.exe");
    std::fs::write(&command_path, b"synthetic executable path marker")
        .unwrap_or_else(|error| panic!("write temp command path: {error}"));
    let params = shell_params(
        &command_path.display().to_string(),
        vec!["--version"],
        30_000,
    );

    println!(
        "readback=act_run_shell_command_shape edge=existing_path_with_spaces before={:?}",
        params.command
    );
    validate_run_shell_params(&params)
        .unwrap_or_else(|error| panic!("existing executable path with spaces rejected: {error}"));
}

#[test]
fn shell_params_allow_working_dir_relative_command_path_with_spaces() {
    let dir = tempfile::TempDir::new()
        .unwrap_or_else(|error| panic!("create temp shell working dir: {error}"));
    let command_name = "tool with spaces.exe";
    let command_path = dir.path().join(command_name);
    std::fs::write(&command_path, b"synthetic executable path marker")
        .unwrap_or_else(|error| panic!("write temp command path: {error}"));
    let mut params = shell_params(command_name, vec!["--version"], 30_000);
    params.working_dir = Some(dir.path().display().to_string());

    println!(
        "readback=act_run_shell_command_shape edge=working_dir_relative_path before={:?} working_dir={:?}",
        params.command, params.working_dir
    );
    validate_run_shell_params(&params).unwrap_or_else(|error| {
        panic!("working_dir-relative executable path with spaces rejected: {error}")
    });
}

#[test]
fn launch_command_line_quotes_empty_and_space_args() {
    let params = launch_params("notepad.exe", vec!["C:\\tmp\\hello world.txt", ""], 10_000);

    assert_eq!(
        launch_command_line(&params).unwrap_or_else(|error| {
            panic!("synthetic launch command line should build: {error}")
        }),
        "notepad.exe \"C:\\tmp\\hello world.txt\" \"\""
    );
}

#[cfg(windows)]
#[test]
fn launch_command_line_uses_win32_long_path_for_existing_path_target() {
    let dir = tempfile::TempDir::new()
        .unwrap_or_else(|error| panic!("create temp launch path dir: {error}"));
    let target_path = dir.path().join("synapse launch target.exe");
    std::fs::write(&target_path, b"synthetic")
        .unwrap_or_else(|error| panic!("write temp target: {error}"));
    let params = launch_params(&target_path.display().to_string(), vec!["--flag"], 10_000);

    let command_line = launch_command_line(&params)
        .unwrap_or_else(|error| panic!("existing path-like launch target should resolve: {error}"));

    assert!(
        command_line.contains("synapse launch target.exe\" --flag"),
        "{command_line}"
    );
}

#[cfg(windows)]
#[test]
fn launch_command_line_rejects_unresolvable_path_target() {
    let dir = tempfile::TempDir::new()
        .unwrap_or_else(|error| panic!("create temp launch path dir: {error}"));
    let target_path = dir.path().join("missing-launch-target.exe");
    let params = launch_params(&target_path.display().to_string(), Vec::new(), 10_000);

    let error = match launch_command_line(&params) {
        Ok(command_line) => panic!("missing path should not resolve, got {command_line}"),
        Err(error) => error,
    };

    assert_eq!(
        error
            .data
            .as_ref()
            .and_then(|data| data.get("reason"))
            .and_then(|reason| reason.as_str()),
        Some("launch_target_path_resolution_failed")
    );
}

#[cfg(windows)]
#[test]
fn launch_desktop_shared_tabbed_targets_require_absolute_path() {
    for target in ["notepad", "notepad.exe"] {
        let mut params = launch_params(target, vec![r"C:\tmp\issue1319.txt"], 10_000);
        params.desktop = Some("agent:session".to_owned());
        params.wait_for_window_title_regex = Some("issue1319".to_owned());

        let error = validate_launch_params(&params)
            .expect_err("pathless shared-tabbed desktop targets must fail closed");

        println!(
            "readback=act_launch_shared_tabbed_desktop_target edge=pathless before=target:{target:?} after={:?}",
            error.data
        );
        assert_eq!(
            error
                .data
                .as_ref()
                .and_then(|data| data.get("reason"))
                .and_then(|reason| reason.as_str()),
            Some("shared_tabbed_app_desktop_requires_explicit_path")
        );
        assert_eq!(
            error
                .data
                .as_ref()
                .and_then(|data| data.get("launch_target_name"))
                .and_then(|name| name.as_str()),
            Some("notepad.exe")
        );
    }

    let mut explicit = launch_params(
        r"C:\Windows\notepad.exe",
        vec![r"C:\tmp\issue1319.txt"],
        10_000,
    );
    explicit.desktop = Some("agent:session".to_owned());
    explicit.wait_for_window_title_regex = Some("issue1319".to_owned());

    println!(
        "readback=act_launch_shared_tabbed_desktop_target happy=explicit_path before=target:{:?}",
        explicit.target
    );
    validate_launch_params(&explicit)
        .expect("absolute shared-tabbed desktop target remains eligible for hidden-desktop wait");
}

#[cfg(windows)]
#[test]
fn launch_target_effective_file_name_matches_windows_createprocess_rules() {
    let cases = [
        ("notepad", "notepad.exe"),
        ("notepad.exe", "notepad.exe"),
        ("notepad.", "notepad."),
        (r"C:\Windows\notepad", "notepad"),
        (r"C:\Windows\notepad.exe", "notepad.exe"),
    ];

    for (target, expected) in cases {
        let actual = launch_target_effective_file_name(target);
        println!(
            "readback=act_launch_effective_target_name before=target:{target:?} after={actual:?}"
        );
        assert_eq!(actual, expected);
    }
}

#[test]
fn launch_window_selection_prefers_new_matching_window() {
    let contexts = vec![
        foreground_for_launch_selection(10, 100, "chrome.exe", "Google Chrome"),
        foreground_for_launch_selection(11, 999, "chrome.exe", "Google Chrome"),
    ];
    let excluded = HashSet::from([10]);
    let title_regex = regex::Regex::new("Chrome|Chromium").expect("synthetic regex compiles");

    let selected = select_launch_window(&contexts, 999, &title_regex, &excluded, "chrome.exe", &[])
        .expect("new matching window should be selected");

    assert_eq!(selected.hwnd, 11);
}

#[test]
fn launch_window_selection_rejects_unowned_new_matching_window() {
    let contexts = vec![foreground_for_launch_selection(
        11,
        200,
        "chrome.exe",
        "Google Chrome",
    )];
    let excluded = HashSet::new();
    let title_regex = regex::Regex::new("Chrome|Chromium").expect("synthetic regex compiles");

    let selected = select_launch_window(&contexts, 999, &title_regex, &excluded, "chrome.exe", &[]);

    assert!(
        selected.is_none(),
        "a matching title from an unrelated PID must not satisfy launch wait"
    );
}

#[test]
fn launch_desktop_window_selection_accepts_new_tabbed_notepad_with_broker_pid() {
    let contexts = vec![foreground_for_launch_selection(
        11,
        39016,
        "Notepad.exe",
        "Untitled - Notepad",
    )];
    let excluded = HashSet::new();
    let title_regex = regex::Regex::new("^Untitled - Notepad$").expect("synthetic regex compiles");

    let selected = select_launch_desktop_window(
        &contexts,
        51028,
        &title_regex,
        &excluded,
        "notepad.exe",
        &[],
    )
    .expect("new hidden-desktop Notepad window should satisfy launch wait despite broker PID");

    assert_eq!(selected.hwnd, 11);
}

#[cfg(windows)]
#[test]
fn launch_desktop_window_selection_accepts_extensionless_notepad_effective_name() {
    let contexts = vec![foreground_for_launch_selection(
        11,
        39016,
        "Notepad.exe",
        "issue1319.txt - Notepad",
    )];
    let excluded = HashSet::new();
    let title_regex =
        regex::Regex::new("^issue1319\\.txt - Notepad$").expect("synthetic regex compiles");
    let launch_target_name = launch_target_effective_file_name("notepad");

    let selected = select_launch_desktop_window(
        &contexts,
        51028,
        &title_regex,
        &excluded,
        &launch_target_name,
        &[r"C:\tmp\issue1319.txt".to_owned()],
    )
    .expect("effective notepad.exe name should match brokered hidden-desktop Notepad window");

    assert_eq!(launch_target_name, "notepad.exe");
    assert_eq!(selected.hwnd, 11);
}

#[test]
fn launch_desktop_window_selection_rejects_excluded_tabbed_notepad() {
    let contexts = vec![foreground_for_launch_selection(
        10,
        39016,
        "Notepad.exe",
        "Untitled - Notepad",
    )];
    let excluded = HashSet::from([10]);
    let title_regex = regex::Regex::new("^Untitled - Notepad$").expect("synthetic regex compiles");

    let selected = select_launch_desktop_window(
        &contexts,
        51028,
        &title_regex,
        &excluded,
        "notepad.exe",
        &[],
    );

    assert!(
        selected.is_none(),
        "pre-existing hidden-desktop Notepad windows must not become owned launch targets"
    );
}

#[test]
fn launch_window_selection_accepts_new_brokered_windows_terminal_window() {
    let contexts = vec![foreground_for_launch_selection(
        11,
        200,
        "WindowsTerminal.exe",
        "Calyx #559 regression",
    )];
    let excluded = HashSet::new();
    let title_regex =
        regex::Regex::new("^Calyx #559 regression$").expect("synthetic regex compiles");

    let selected = select_launch_window(&contexts, 999, &title_regex, &excluded, "wt.exe", &[])
        .expect("new brokered Windows Terminal window should satisfy launch wait");

    assert_eq!(selected.hwnd, 11);
}

#[test]
fn launch_window_selection_accepts_existing_single_instance_window() {
    let contexts = vec![foreground_for_launch_selection(
        10,
        100,
        "chrome.exe",
        "Google Chrome",
    )];
    let excluded = HashSet::from([10]);
    let title_regex = regex::Regex::new("Chrome|Chromium").expect("synthetic regex compiles");

    let selected = select_launch_window(&contexts, 999, &title_regex, &excluded, "chrome.exe", &[])
        .expect("existing single-instance matching window should be selected");

    assert_eq!(selected.hwnd, 10);
}

#[test]
fn launch_window_selection_rejects_existing_tabbed_notepad_window() {
    let contexts = vec![foreground_for_launch_selection(
        10,
        100,
        "Notepad.exe",
        "issue1034-qwen8v2-notepad.txt - Notepad",
    )];
    let excluded = HashSet::from([10]);
    let title_regex = regex::Regex::new("Notepad").expect("synthetic regex compiles");

    let selected = select_launch_window(
        &contexts,
        999,
        &title_regex,
        &excluded,
        "notepad.exe",
        &["C:\\tmp\\issue1271.txt".to_owned()],
    );

    assert!(
        selected.is_none(),
        "existing Notepad tab hosts must not become agent-owned launch targets"
    );
}

#[test]
fn launch_window_selection_accepts_new_tabbed_notepad_window_from_spawn_pid() {
    let contexts = vec![
        foreground_for_launch_selection(10, 100, "Notepad.exe", "User Notes - Notepad"),
        foreground_for_launch_selection(11, 999, "Notepad.exe", "issue1271.txt - Notepad"),
    ];
    let excluded = HashSet::from([10]);
    let title_regex =
        regex::Regex::new("issue1271\\.txt - Notepad").expect("synthetic regex compiles");

    let selected = select_launch_window(
        &contexts,
        999,
        &title_regex,
        &excluded,
        "notepad.exe",
        &["C:\\tmp\\issue1271.txt".to_owned()],
    )
    .expect("new Notepad window owned by the spawned PID should be selected");

    assert_eq!(selected.hwnd, 11);
}

#[test]
fn launch_window_selection_rejects_unrelated_existing_window() {
    let contexts = vec![foreground_for_launch_selection(
        10,
        100,
        "WindowsTerminal.exe",
        "Synapse - Windows Terminal",
    )];
    let excluded = HashSet::from([10]);
    let title_regex = regex::Regex::new("Synapse|Explorer").expect("synthetic regex compiles");

    let selected =
        select_launch_window(&contexts, 999, &title_regex, &excluded, "explorer.exe", &[]);

    assert!(
        selected.is_none(),
        "unrelated existing windows must not satisfy broad launch title regexes"
    );
}

#[test]
fn launch_window_selection_accepts_known_shell_activation_window() {
    let contexts = vec![foreground_for_launch_selection(
        10,
        100,
        "ApplicationFrameHost.exe",
        "Settings",
    )];
    let excluded = HashSet::from([10]);
    let title_regex =
        regex::Regex::new("^(Settings|Control Panel)$").expect("synthetic regex compiles");
    let launch_args = vec!["ms-settings:".to_owned()];

    let selected = select_launch_window(
        &contexts,
        999,
        &title_regex,
        &excluded,
        "explorer.exe",
        &launch_args,
    )
    .expect("known shell-activated app window should be accepted");

    assert_eq!(selected.hwnd, 10);
}

#[test]
fn shell_allowlist_accepts_narrow_startup_patterns() {
    let config = M4ServiceConfig::from_cli_parts(
        vec![
            r"^git \w+$".to_owned(),
            r"^echo .{0,100}$".to_owned(),
            r"^cargo (build|test)( --[\w-]+)*$".to_owned(),
        ],
        Vec::new(),
        DEFAULT_RUN_SHELL_INLINE_AWAIT_LIMIT_MS,
    );

    assert!(
        config.is_ok(),
        "narrow allow-shell examples should compile: {config:?}"
    );
}

#[test]
fn shell_allowlist_rejects_broad_startup_patterns() {
    let cases = [
        ("", "empty_pattern"),
        (".*", "unbounded_any_character_repetition"),
        ("^.+$", "unbounded_any_character_repetition"),
        ("^$", "matches_empty"),
        ("git status", "shell_pattern_must_match_full_command_line"),
        (r"^[\s\S]*$", "unbounded_any_character_repetition"),
    ];

    for (pattern, reason) in cases {
        let error = match M4ServiceConfig::from_cli_parts(
            vec![pattern.to_owned()],
            Vec::new(),
            DEFAULT_RUN_SHELL_INLINE_AWAIT_LIMIT_MS,
        ) {
            Ok(config) => panic!("pattern {pattern:?} should reject, got {config:?}"),
            Err(error) => error,
        };
        let Some(broad) = error.downcast_ref::<BroadAllowPatternError>() else {
            panic!("pattern {pattern:?} returned unexpected error: {error:#}");
        };
        assert_eq!(broad.reason(), reason);
    }
}

#[tokio::test]
async fn shell_denies_without_allowlist() {
    let params = shell_params("synthetic-shell-denied", Vec::new(), 30_000);

    let error = match run_shell(&M4ServiceConfig::default(), params).await {
        Ok(response) => panic!("unallowlisted shell should deny, got {response:?}"),
        Err(error) => error,
    };

    assert_eq!(
        error
            .data
            .as_ref()
            .and_then(|data| data.get("code"))
            .and_then(|code| code.as_str()),
        Some(error_codes::SAFETY_SHELL_DENIED_BY_POLICY)
    );
    assert_eq!(
        error
            .data
            .as_ref()
            .and_then(|data| data.get("reason"))
            .and_then(|reason| reason.as_str()),
        Some("no_allow_shell_policy")
    );
}

#[tokio::test]
async fn launch_denies_without_allowlist() {
    let params = launch_params("synthetic-launch-denied", Vec::new(), 10_000);

    let error = match launch(&M4ServiceConfig::default(), params).await {
        Ok(response) => panic!("unallowlisted launch should deny, got {response:?}"),
        Err(error) => error,
    };

    assert_eq!(
        error
            .data
            .as_ref()
            .and_then(|data| data.get("code"))
            .and_then(|code| code.as_str()),
        Some(error_codes::SAFETY_LAUNCH_DENIED_BY_POLICY)
    );
    assert_eq!(
        error
            .data
            .as_ref()
            .and_then(|data| data.get("reason"))
            .and_then(|reason| reason.as_str()),
        Some("no_allow_launch_policy")
    );
}

#[cfg(windows)]
#[tokio::test]
async fn launch_applies_working_dir_and_env() {
    let dir = match tempfile::TempDir::new() {
        Ok(dir) => dir,
        Err(error) => panic!("create temp launch dir: {error}"),
    };
    let output_path = dir.path().join("launch-env.txt");
    let mut params = launch_params(
        "cmd.exe",
        vec!["/c", "echo %SYNAPSE_LAUNCH_ENV%>launch-env.txt"],
        10_000,
    );
    params.working_dir = Some(dir.path().display().to_string());
    params.env.insert(
        "SYNAPSE_LAUNCH_ENV".to_owned(),
        "synapse-launch-ok".to_owned(),
    );
    let config = launch_config_for(&params);

    let response = match launch(&config, params).await {
        Ok(response) => response,
        Err(error) => panic!("allowlisted cmd launch should spawn: {error}"),
    };

    assert!(response.pid > 0);
    assert_eq!(response.hwnd, None);
    assert_eq!(response.matched_title, None);
    assert_eq!(response.reason, None);
    let text = read_text_file_with_retry(&output_path).await;
    assert_eq!(text.trim(), "synapse-launch-ok");
}

#[cfg(windows)]
#[tokio::test]
async fn launch_wait_refuses_console_window_title_wait() {
    // Console launches are hidden/no-window by policy, so a window-title
    // wait on a console target is unsatisfiable and must fail closed
    // before spawning anything (validate_console_launch_visibility).
    let mut params = launch_params("cmd.exe", vec!["/c", "exit 0"], 50);
    params.wait_for_window_title_regex = Some("^SynapseLaunchNoSuchWindow$".to_owned());
    let config = launch_config_for(&params);

    let error = match launch(&config, params).await {
        Ok(response) => panic!("console window wait should fail closed: {response:?}"),
        Err(error) => error,
    };

    println!(
        "readback=act_launch_window_wait edge=console_no_window before=regex:^SynapseLaunchNoSuchWindow$ after=error:{error}"
    );
    assert_eq!(
        error
            .data
            .as_ref()
            .and_then(|data| data.get("code"))
            .and_then(|code| code.as_str()),
        Some(error_codes::TOOL_PARAMS_INVALID)
    );
    assert_eq!(
        error
            .data
            .as_ref()
            .and_then(|data| data.get("reason"))
            .and_then(|reason| reason.as_str()),
        Some("hidden_console_has_no_window_to_wait_for")
    );
}

#[cfg(windows)]
async fn read_text_file_with_retry(path: &std::path::Path) -> String {
    for _ in 0..100 {
        match std::fs::read_to_string(path) {
            Ok(text) => return text,
            Err(_error) => tokio::time::sleep(Duration::from_millis(20)).await,
        }
    }
    panic!(
        "file {} was not created by launched process",
        path.display()
    );
}

#[cfg(windows)]
#[tokio::test]
async fn shell_allows_cmd_echo_and_captures_stdout() {
    let params = shell_params("cmd.exe", vec!["/c", "echo synapse-m4-shell-ok"], 30_000);
    let config = shell_config_for(&params);

    let response = match run_shell(&config, params).await {
        Ok(response) => response,
        Err(error) => panic!("allowlisted cmd echo should run: {error}"),
    };

    assert_eq!(response.exit_code, Some(0));
    assert_eq!(response.stdout, "synapse-m4-shell-ok\r\n");
    assert_eq!(response.stderr, "");
    assert!(!response.timed_out);
    assert!(!response.stdout_truncated);
    assert!(!response.stderr_truncated);
}

#[cfg(windows)]
#[tokio::test]
async fn shell_caps_stdout_and_marks_truncated() {
    let params = shell_params(
        "powershell.exe",
        vec![
            "-NoProfile",
            "-Command",
            "[Console]::Out.Write(('x'*1048580))",
        ],
        30_000,
    );
    let config = shell_config_for(&params);

    let response = match run_shell(&config, params).await {
        Ok(response) => response,
        Err(error) => panic!("allowlisted large stdout command should run: {error}"),
    };

    assert_eq!(response.exit_code, Some(0));
    assert_eq!(response.stdout.len(), SHELL_OUTPUT_CAP_BYTES);
    assert!(response.stdout.chars().all(|ch| ch == 'x'));
    assert!(response.stdout_truncated);
    assert_eq!(response.stderr, "");
    assert!(!response.stderr_truncated);
    assert!(!response.timed_out);
}

#[cfg(windows)]
#[tokio::test]
#[ignore = "real-process FSV: spawns + tree-kills a real powershell.exe; the spawn/kill wall-clock balloons on a saturated host (run explicitly with `cargo test -p synapse-mcp -- --ignored`). See M4_ACT_RUN_SHELL timeout-path perf follow-up."]
async fn shell_timeout_kills_process_and_marks_timed_out() {
    let params = shell_params(
        "powershell.exe",
        vec!["-NoProfile", "-Command", "Start-Sleep -Milliseconds 5000"],
        500,
    );
    let config = shell_config_for(&params);

    let response = match run_shell(&config, params).await {
        Ok(response) => response,
        Err(error) => {
            panic!("allowlisted sleeping command should return timeout response: {error}")
        }
    };

    assert_eq!(response.exit_code, None);
    assert!(response.timed_out);
    assert_eq!(
        response.error_code.as_deref(),
        Some(error_codes::ACTION_BUDGET_EXPIRED)
    );
    assert!(
        response
            .error_message
            .as_deref()
            .is_some_and(|message| message.contains("500 ms")),
        "{response:?}"
    );
    // Correctness here is the timeout *firing* and the call returning at all —
    // proven by `timed_out`, the ACTION_BUDGET_EXPIRED code, and the "500 ms"
    // message above, all of which are load-independent. We deliberately do NOT
    // assert a wall-clock bound on `duration_ms`: spawning and tree-killing a
    // real powershell.exe is an OS-scheduling cost that balloons on a saturated
    // host, so any fixed bound is flaky as a gate. Protection against the call
    // *hanging indefinitely* on a wedged inherited pipe lives in the production
    // path (SHELL_READER_DRAIN_CAP), not in a timing assertion here.
}

#[test]
fn shell_accepts_timeout_above_legacy_cap() {
    let params = shell_params("cmd.exe", vec!["/c", "echo long-timeout-ok"], 1_200_000);

    let authorization = authorize_run_shell(&shell_config_for(&params), &params)
        .unwrap_or_else(|error| panic!("legacy-cap timeout should authorize: {error}"));

    assert_eq!(
        authorization.command_line,
        "cmd.exe /c \"echo long-timeout-ok\""
    );
}

#[test]
fn act_run_shell_deserializes_null_timeout_fields_as_omitted() {
    let omitted: ActRunShellParams = serde_json::from_value(json!({
        "command": "powershell.exe",
        "args": ["-NoProfile", "-Command", "Write-Output omitted"],
        "execution_mode": "durable"
    }))
    .unwrap_or_else(|error| panic!("omitted timeout fields should deserialize: {error}"));
    let explicit_null: ActRunShellParams = serde_json::from_value(json!({
        "command": "powershell.exe",
        "args": ["-NoProfile", "-Command", "Write-Output null"],
        "execution_mode": "durable",
        "timeout_ms": null,
        "durable_timeout_ms": null
    }))
    .unwrap_or_else(|error| panic!("null timeout fields should deserialize: {error}"));

    println!(
        "readback=act_run_shell_params edge=null_timeouts before=omitted after=timeout_ms:{} durable_timeout_ms:{:?}",
        explicit_null.timeout_ms, explicit_null.durable_timeout_ms
    );
    assert_eq!(omitted.timeout_ms, default_shell_timeout_ms());
    assert_eq!(explicit_null.timeout_ms, default_shell_timeout_ms());
    assert_eq!(explicit_null.durable_timeout_ms, None);
}

#[test]
fn act_run_shell_invalid_timeout_type_still_fails_deserialization() {
    let error = serde_json::from_value::<ActRunShellParams>(json!({
        "command": "powershell.exe",
        "args": ["-NoProfile", "-Command", "Write-Output invalid"],
        "execution_mode": "durable",
        "timeout_ms": "not-a-number"
    }))
    .expect_err("invalid concrete timeout type must fail");

    println!("readback=act_run_shell_params edge=invalid_timeout_type after=error:{error}");
    assert!(error.to_string().contains("invalid type"));
}

#[test]
fn act_run_shell_zero_timeout_still_fails_validation() {
    let params: ActRunShellParams = serde_json::from_value(json!({
        "command": "powershell.exe",
        "args": ["-NoProfile", "-Command", "Write-Output zero"],
        "execution_mode": "durable",
        "timeout_ms": 0
    }))
    .unwrap_or_else(|error| panic!("zero timeout should deserialize for validation: {error}"));
    let error = validate_run_shell_params(&params)
        .expect_err("zero timeout must fail closed in validation");

    println!(
        "readback=act_run_shell_params edge=zero_timeout after=error:{}",
        error.message
    );
    assert_eq!(extract_error_code(&error), error_codes::TOOL_PARAMS_INVALID);
    assert!(error.message.contains("timeout_ms must be >= 1"));
}

#[cfg(windows)]
#[tokio::test]
async fn shell_long_timeout_returns_durable_job_handle() {
    // Hermetic durable-job root (#1610): this test backgrounds a durable job
    // and polls its status, so without an isolated root a sibling shell test's
    // session cleanup / reap / enumeration of the shared process-global root
    // (`%LOCALAPPDATA%\Synapse\shell-jobs`) can reconcile or race this job dir
    // under parallel `m4` runs. The `ShellJobRootGuard` gives it a private root.
    let _root_guard = ShellJobRootGuard::new();
    let inline_await_limit_ms = 1;
    let timeout_ms = DEFAULT_SHELL_TIMEOUT_MS;
    let params = shell_params(
        "cmd.exe",
        vec!["/c", "echo background-handoff-ok"],
        timeout_ms,
    );
    let mut config = shell_config_for(&params);
    config.run_shell_inline_await_limit_ms = inline_await_limit_ms;

    let response = match run_shell(&config, params).await {
        Ok(response) => response,
        Err(error) => panic!("long direct shell call should return job handle: {error}"),
    };

    println!("readback=act_run_shell edge=long_timeout_handoff after=response:{response:?}");
    assert!(response.backgrounded);
    assert_eq!(
        response.background_reason.as_deref(),
        Some("timeout_exceeds_inline_await_budget")
    );
    assert_eq!(response.inline_await_limit_ms, Some(inline_await_limit_ms));
    assert_eq!(response.exit_code, None);
    assert_eq!(response.stdout, "");
    assert_eq!(response.stderr, "");
    let job_id = response
        .job_id
        .clone()
        .expect("background response should include job id");
    let job = response
        .job
        .expect("background response should include job");
    assert_eq!(job.job_id, job_id);
    assert_eq!(job.status, "running");
    assert_eq!(job.timeout_ms, None);

    for _ in 0..100 {
        let status = shell_job_status(
            &ActRunShellStatusParams {
                job_id: job_id.clone(),
                tail_bytes: 4096,
            },
            None,
        )
        .unwrap_or_else(|error| panic!("status should read durable job state: {error}"));
        println!("readback=act_run_shell edge=long_timeout_handoff after=status:{status:?}");
        if status.job.status == "finalizing" {
            tokio::time::sleep(Duration::from_millis(25)).await;
            continue;
        }
        if !status.running {
            assert_eq!(status.job.status, "ok");
            assert_eq!(status.job.exit_code, Some(0));
            assert!(status.stdout_tail.contains("background-handoff-ok"));
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    panic!("background job {job_id} did not complete within the regression readback window");
}

#[cfg(windows)]
#[tokio::test]
async fn shell_inline_mode_honors_timeout_above_auto_background_limit() {
    let inline_await_limit_ms = 1;
    let mut params = shell_params(
        "cmd.exe",
        vec!["/c", "echo inline-override-ok"],
        DEFAULT_SHELL_TIMEOUT_MS,
    );
    params.execution_mode = ActRunShellExecutionMode::Inline;
    let mut config = shell_config_for(&params);
    config.run_shell_inline_await_limit_ms = inline_await_limit_ms;

    let response = match run_shell(&config, params).await {
        Ok(response) => response,
        Err(error) => panic!("inline execution mode should not auto-background: {error}"),
    };

    println!("readback=act_run_shell edge=inline_mode_above_limit after=response:{response:?}");
    assert_eq!(response.exit_code, Some(0));
    assert_eq!(response.stdout, "inline-override-ok\r\n");
    assert!(!response.backgrounded);
    assert_eq!(
        response.requested_execution_mode,
        Some(ActRunShellExecutionMode::Inline)
    );
    assert_eq!(
        response.effective_execution_mode,
        Some(ActRunShellExecutionMode::Inline)
    );
    assert_eq!(response.job_id, None);
}

#[cfg(windows)]
#[tokio::test]
async fn shell_inline_mode_waits_past_inline_await_limit() {
    // Regression for #954: a command that runs LONGER than the daemon inline await limit must
    // still complete inline when execution_mode="inline" and the requested wait fits inside
    // the MCP client-call budget. The inline await limit only governs the auto→durable
    // background decision.
    let inline_await_limit_ms = 200;
    let mut params = shell_params(
        "powershell.exe",
        vec![
            "-NoProfile",
            "-Command",
            "Start-Sleep -Milliseconds 600; Write-Output inline-slow-ok",
        ],
        10_000,
    );
    params.execution_mode = ActRunShellExecutionMode::Inline;
    let mut config = shell_config_for(&params);
    config.run_shell_inline_await_limit_ms = inline_await_limit_ms;

    println!(
        "readback=act_run_shell edge=inline_waits_past_limit before=inline_await_limit_ms:{inline_await_limit_ms} command_runtime_ms:~600"
    );
    let response = match run_shell(&config, params).await {
        Ok(response) => response,
        Err(error) => panic!("inline execution past the await limit should complete: {error}"),
    };

    println!("readback=act_run_shell edge=inline_waits_past_limit after=response:{response:?}");
    assert!(
        !response.timed_out,
        "command shorter than timeout_ms must not be killed by the inline await limit: {response:?}"
    );
    assert_eq!(response.exit_code, Some(0), "{response:?}");
    assert!(!response.backgrounded, "{response:?}");
    assert!(
        response.stdout.contains("inline-slow-ok"),
        "command must run to completion inline: {response:?}"
    );
    assert!(
        response.duration_ms >= inline_await_limit_ms as u32,
        "execution must have outlasted the {inline_await_limit_ms} ms inline await limit: {response:?}"
    );
    assert!(response.error_code.is_none(), "{response:?}");
}

#[cfg(windows)]
#[tokio::test]
async fn shell_inline_timeout_above_client_budget_returns_durable_job_handle() {
    // Hermetic durable-job root (#1610): backgrounds a durable job and awaits
    // its completion, so isolate it from sibling shell tests sharing the
    // process-global root under parallel `m4` runs.
    let _root_guard = ShellJobRootGuard::new();
    let mut params = shell_params(
        "cmd.exe",
        vec!["/c", "echo inline-client-budget-handoff-ok"],
        DEFAULT_RUN_SHELL_INLINE_CLIENT_CALL_BUDGET_MS + 1,
    );
    params.execution_mode = ActRunShellExecutionMode::Inline;
    let config = shell_config_for(&params);

    let response = match run_shell(&config, params).await {
        Ok(response) => response,
        Err(error) => panic!("oversized inline request should return durable handle: {error}"),
    };

    println!(
        "readback=act_run_shell edge=inline_client_budget_handoff after=response:{response:?}"
    );
    assert!(response.backgrounded, "{response:?}");
    assert_eq!(
        response.background_reason.as_deref(),
        Some("inline_timeout_exceeds_mcp_client_call_budget")
    );
    assert_eq!(
        response.inline_client_call_budget_ms,
        Some(DEFAULT_RUN_SHELL_INLINE_CLIENT_CALL_BUDGET_MS)
    );
    assert_eq!(
        response.requested_execution_mode,
        Some(ActRunShellExecutionMode::Inline)
    );
    assert_eq!(
        response.effective_execution_mode,
        Some(ActRunShellExecutionMode::Durable)
    );
    let job_id = response.job_id.clone().expect("job id should be returned");
    assert_durable_job_finishes_ok(&job_id, "inline-client-budget-handoff-ok").await;
}

#[cfg(windows)]
#[tokio::test]
async fn shell_budget_expiry_message_is_actionable() {
    // When the caller's own timeout_ms budget expires, the error must point at the concrete
    // escape hatch (durable execution / the inline await limit) instead of dead-ending.
    let mut params = shell_params(
        "powershell.exe",
        vec!["-NoProfile", "-Command", "Start-Sleep -Milliseconds 5000"],
        400,
    );
    params.execution_mode = ActRunShellExecutionMode::Auto;
    let config = shell_config_for(&params);

    let response = match run_shell(&config, params).await {
        Ok(response) => response,
        Err(error) => {
            panic!("expired-budget command should return a timeout response: {error}")
        }
    };

    println!("readback=act_run_shell edge=actionable_budget_error after=response:{response:?}");
    assert!(response.timed_out, "{response:?}");
    assert_eq!(
        response.error_code.as_deref(),
        Some(error_codes::ACTION_BUDGET_EXPIRED),
        "{response:?}"
    );
    let message = response
        .error_message
        .as_deref()
        .expect("expired budget must carry an error message");
    assert!(
        message.contains("400 ms"),
        "names the expired budget: {message}"
    );
    assert!(
        message.contains("execution_mode=\"durable\""),
        "names the durable escape hatch: {message}"
    );
    assert!(
        message.contains("inline await limit"),
        "names the configurable inline await limit: {message}"
    );
    assert!(
        message.contains("MCP client-call budget"),
        "names the client-call guard: {message}"
    );
}

/// Seed a synthetic durable shell job on disk with a fully-formed status
/// record so the reaper's real `read_shell_job_status` path exercises it.
/// `completed_at` is the retention clock; `None` models a still-live job.
#[cfg(test)]
fn seed_synthetic_shell_job(
    root: &Path,
    job_id: &str,
    status: &str,
    started_at: &str,
    completed_at: Option<&str>,
) -> ShellJobPaths {
    let paths = shell_job_paths_from_root(root, job_id);
    fs::create_dir_all(&paths.job_dir)
        .unwrap_or_else(|error| panic!("seed job dir {job_id} should create: {error}"));
    let params = ActRunShellStartParams {
        command: "powershell.exe".to_owned(),
        args: vec![
            "-NoProfile".to_owned(),
            "-Command".to_owned(),
            "Write-Output reap-seed".to_owned(),
        ],
        working_dir: None,
        env: BTreeMap::new(),
        timeout_ms: Some(30_000),
        job_id: Some(job_id.to_owned()),
    };
    let authorization = RunShellAuthorization {
        command_line: shell_command_line_from_parts(&params.command, &params.args),
        matched_pattern: "__any_permitted__".to_owned(),
    };
    let request_sha = run_shell_start_request_sha256(&params)
        .unwrap_or_else(|error| panic!("seed request should hash: {error}"));
    let mut record = shell_job_status_record(
        job_id,
        status,
        &params,
        &paths,
        &request_sha,
        &authorization,
        started_at.to_owned(),
        Some(4242),
        None,
    );
    record.completed_at = completed_at.map(ToOwned::to_owned);
    if completed_at.is_some() {
        record.exit_code = Some(0);
        record.duration_ms = Some(10);
    }
    // Give each job a little stdout so bytes_reclaimed is provably non-zero.
    fs::write(&paths.stdout_path, b"reap-seed-stdout\n")
        .unwrap_or_else(|error| panic!("seed stdout {job_id} should write: {error}"));
    write_shell_job_status(&paths.status_path, &record)
        .unwrap_or_else(|error| panic!("seed status {job_id} should write: {error}"));
    paths
}

// #1510 full-state verification: seed synthetic jobs with backdated
// completion timestamps and prove — by reading the filesystem, the source of
// truth — that only aged *terminal* jobs are removed while live, finalizing,
// recently-terminal, and unreadable jobs survive.
#[test]
fn reap_stale_shell_jobs_removes_only_aged_terminal_jobs() {
    let _root_guard = ShellJobRootGuard::new();
    let root = shell_durable_job_root_dir()
        .unwrap_or_else(|error| panic!("durable root should resolve: {error}"));
    fs::create_dir_all(&root).unwrap_or_else(|error| panic!("durable root should create: {error}"));

    let old = (chrono::Utc::now() - chrono::Duration::days(30)).to_rfc3339();
    let recent = chrono::Utc::now().to_rfc3339();

    // Reap candidates: two distinct terminal statuses, both aged out.
    let old_ok = seed_synthetic_shell_job(&root, "reap-old-ok", "ok", &old, Some(&old));
    let old_exited = seed_synthetic_shell_job(
        &root,
        "reap-old-exited",
        "exited_unobserved",
        &old,
        Some(&old),
    );
    // Reap candidate: a `finalizing` job stuck far past the millisecond-scale
    // finalize window is abandoned and must be reaped like any settled job
    // (the exact leak observed in the real store, #1510).
    let old_finalizing =
        seed_synthetic_shell_job(&root, "reap-old-finalizing", "finalizing", &old, Some(&old));
    // Retain: recently completed terminal job (age below TTL).
    let recent_ok = seed_synthetic_shell_job(&root, "reap-recent-ok", "ok", &recent, Some(&recent));
    // Retain: a genuinely in-flight `finalizing` job (just now) — the age
    // guard protects the millisecond window while the monitor writes final
    // output.
    let recent_finalizing = seed_synthetic_shell_job(
        &root,
        "reap-recent-finalizing",
        "finalizing",
        &recent,
        Some(&recent),
    );
    // Retain: live job (running) — never reaped regardless of age.
    let running = seed_synthetic_shell_job(&root, "reap-running", "running", &old, None);
    // Retain: unreadable status file — cannot prove it is settled, never reaped.
    let corrupt_paths = shell_job_paths_from_root(&root, "reap-corrupt");
    fs::create_dir_all(&corrupt_paths.job_dir)
        .unwrap_or_else(|error| panic!("corrupt job dir should create: {error}"));
    fs::write(&corrupt_paths.status_path, b"{ this is not valid json")
        .unwrap_or_else(|error| panic!("corrupt status should write: {error}"));

    // Precondition (source of truth = filesystem): all seven dirs exist.
    for dir in [
        &old_ok.job_dir,
        &old_exited.job_dir,
        &old_finalizing.job_dir,
        &recent_ok.job_dir,
        &recent_finalizing.job_dir,
        &running.job_dir,
        &corrupt_paths.job_dir,
    ] {
        assert!(dir.exists(), "precondition: {} should exist", dir.display());
    }

    // Exercise the real public entry point (default 7-day TTL); the aged
    // jobs are 30 days old so they exceed it, the recent one does not.
    let readback =
        reap_stale_shell_jobs().unwrap_or_else(|error| panic!("reap should succeed: {error}"));
    println!("readback=shell_job_reap edge=mixed_store after={readback:?}");

    // Full-state verification against the filesystem itself.
    assert!(
        !old_ok.job_dir.exists(),
        "aged terminal ok job must be gone from disk"
    );
    assert!(
        !old_exited.job_dir.exists(),
        "aged terminal exited job must be gone from disk"
    );
    assert!(
        !old_finalizing.job_dir.exists(),
        "aged abandoned finalizing job must be gone from disk"
    );
    assert!(
        recent_ok.job_dir.exists(),
        "recently-terminal job must be retained on disk"
    );
    assert!(
        recent_finalizing.job_dir.exists(),
        "in-flight finalizing job must be retained on disk"
    );
    assert!(
        running.job_dir.exists(),
        "live running job must be retained on disk"
    );
    assert!(
        corrupt_paths.job_dir.exists(),
        "unreadable job must be retained on disk"
    );

    // Readback accounting must exactly partition the seven scanned dirs.
    assert_eq!(readback.scanned_job_dirs, 7, "readback: {readback:?}");
    assert_eq!(readback.reaped_stale_jobs, 3, "readback: {readback:?}");
    assert_eq!(readback.retained_live_jobs, 1, "readback: {readback:?}");
    assert_eq!(
        readback.retained_recent_terminal_jobs, 2,
        "readback: {readback:?}"
    );
    assert_eq!(
        readback.skipped_unreadable_status_files, 1,
        "readback: {readback:?}"
    );
    assert_eq!(readback.reap_failures, 0, "readback: {readback:?}");
    assert!(
        readback.bytes_reclaimed > 0,
        "reaped jobs had on-disk artifacts: {readback:?}"
    );
    let mut sample = readback.reaped_job_ids_sample.clone();
    sample.sort();
    assert_eq!(
        sample,
        vec!["reap-old-exited", "reap-old-finalizing", "reap-old-ok"]
    );

    // Idempotence: a second pass finds nothing new to reap.
    let second = reap_stale_shell_jobs()
        .unwrap_or_else(|error| panic!("second reap should succeed: {error}"));
    assert_eq!(second.reaped_stale_jobs, 0, "second pass: {second:?}");
    assert_eq!(second.scanned_job_dirs, 4, "second pass: {second:?}");
}

// #1510 boundary audit: the TTL is honored, not hardcoded. The same
// ~2-hour-old terminal job is reaped under a 1-hour TTL and retained under a
// 1-day TTL.
#[test]
fn reap_stale_shell_jobs_honors_ttl_boundary() {
    let _root_guard = ShellJobRootGuard::new();
    let root = shell_durable_job_root_dir()
        .unwrap_or_else(|error| panic!("durable root should resolve: {error}"));
    fs::create_dir_all(&root).unwrap_or_else(|error| panic!("durable root should create: {error}"));
    let two_hours_ago = (chrono::Utc::now() - chrono::Duration::hours(2)).to_rfc3339();

    let under_short_ttl = seed_synthetic_shell_job(
        &root,
        "reap-ttl-short",
        "ok",
        &two_hours_ago,
        Some(&two_hours_ago),
    );
    let short = reap_stale_shell_jobs_with_ttl(Duration::from_hours(1))
        .unwrap_or_else(|error| panic!("short-ttl reap should succeed: {error}"));
    assert!(
        !under_short_ttl.job_dir.exists(),
        "2h-old job must be reaped under a 1h TTL: {short:?}"
    );
    assert_eq!(short.reaped_stale_jobs, 1, "short-ttl readback: {short:?}");

    let under_long_ttl = seed_synthetic_shell_job(
        &root,
        "reap-ttl-long",
        "ok",
        &two_hours_ago,
        Some(&two_hours_ago),
    );
    let long = reap_stale_shell_jobs_with_ttl(Duration::from_hours(24))
        .unwrap_or_else(|error| panic!("long-ttl reap should succeed: {error}"));
    assert!(
        under_long_ttl.job_dir.exists(),
        "2h-old job must be retained under a 1d TTL: {long:?}"
    );
    assert_eq!(long.reaped_stale_jobs, 0, "long-ttl readback: {long:?}");
    assert_eq!(
        long.retained_recent_terminal_jobs, 1,
        "long-ttl readback: {long:?}"
    );
}

#[cfg(windows)]
#[tokio::test]
async fn shell_auto_background_uses_explicit_durable_timeout() {
    // Hermetic durable-job root (#1610): this test backgrounds a durable job
    // and awaits its completion on the process-global shell-jobs root. Under a
    // parallel `m4` run a sibling shell test's session cleanup / reaper /
    // enumeration of that shared root could reconcile or race this job dir,
    // flipping its observed status — the confirmed root cause of the flake.
    let _root_guard = ShellJobRootGuard::new();
    let inline_await_limit_ms = 1;
    let mut params = shell_params(
        "cmd.exe",
        vec!["/c", "echo auto-durable-timeout-ok"],
        DEFAULT_SHELL_TIMEOUT_MS,
    );
    params.durable_timeout_ms = Some(5_000);
    let mut config = shell_config_for(&params);
    config.run_shell_inline_await_limit_ms = inline_await_limit_ms;

    let response = match run_shell(&config, params).await {
        Ok(response) => response,
        Err(error) => panic!("auto background with durable timeout should run: {error}"),
    };

    println!("readback=act_run_shell edge=auto_background_timeout after=response:{response:?}");
    assert!(response.backgrounded);
    assert_eq!(
        response.background_reason.as_deref(),
        Some("timeout_exceeds_inline_await_budget")
    );
    assert_eq!(response.durable_timeout_ms, Some(5_000));
    let job_id = response.job_id.clone().expect("job id should be returned");
    let job = response.job.expect("job should be returned");
    assert_eq!(job.timeout_ms, Some(5_000));
    assert_durable_job_finishes_ok(&job_id, "auto-durable-timeout-ok").await;
}

#[cfg(windows)]
#[tokio::test]
async fn shell_durable_mode_returns_job_without_inline_limit() {
    // Hermetic durable-job root (#1610): same isolation as its sibling durable
    // tests so a parallel shell test cannot reconcile or race this job dir on
    // the shared process-global root.
    let _root_guard = ShellJobRootGuard::new();
    let inline_await_limit_ms = DEFAULT_RUN_SHELL_INLINE_AWAIT_LIMIT_MS;
    let mut params = shell_params(
        "cmd.exe",
        vec!["/c", "echo explicit-durable-mode-ok"],
        DEFAULT_SHELL_TIMEOUT_MS,
    );
    params.execution_mode = ActRunShellExecutionMode::Durable;
    params.durable_timeout_ms = Some(5_000);
    let mut config = shell_config_for(&params);
    config.run_shell_inline_await_limit_ms = inline_await_limit_ms;

    let response = match run_shell(&config, params).await {
        Ok(response) => response,
        Err(error) => panic!("durable execution mode should return a job handle: {error}"),
    };

    println!("readback=act_run_shell edge=durable_mode after=response:{response:?}");
    assert!(response.backgrounded);
    assert_eq!(
        response.background_reason.as_deref(),
        Some("execution_mode_durable")
    );
    assert_eq!(response.inline_await_limit_ms, Some(inline_await_limit_ms));
    assert_eq!(
        response.requested_execution_mode,
        Some(ActRunShellExecutionMode::Durable)
    );
    assert_eq!(
        response.effective_execution_mode,
        Some(ActRunShellExecutionMode::Durable)
    );
    let job_id = response.job_id.clone().expect("job id should be returned");
    let job = response.job.expect("job should be returned");
    assert_eq!(job.timeout_ms, Some(5_000));
    assert_durable_job_finishes_ok(&job_id, "explicit-durable-mode-ok").await;
}

#[cfg(windows)]
#[tokio::test]
async fn shell_inline_ignores_durable_timeout_when_execution_stays_inline() {
    let mut params = shell_params(
        "cmd.exe",
        vec!["/c", "echo inline-durable-timeout-ignored"],
        30_000,
    );
    params.execution_mode = ActRunShellExecutionMode::Inline;
    params.durable_timeout_ms = Some(5_000);
    let config = shell_config_for(&params);
    let details = run_shell_request_details(&params, config.run_shell_inline_await_limit_ms());

    println!("readback=act_run_shell edge=inline_plus_durable_timeout before={details}");
    assert_eq!(details["will_background"], false);
    assert_eq!(
        details["durable_timeout_policy"],
        "ignored_inline_execution"
    );
    assert_eq!(details["durable_timeout_ms"], 5_000);
    assert!(details["durable_timeout_ms_if_backgrounded"].is_null());

    let response = match run_shell(&config, params).await {
        Ok(response) => response,
        Err(error) => panic!("inline durable timeout should be ignored inline: {error}"),
    };

    println!("readback=act_run_shell edge=inline_plus_durable_timeout after_response={response:?}");
    assert!(!response.backgrounded);
    assert_eq!(response.exit_code, Some(0));
    assert!(response.stdout.contains("inline-durable-timeout-ignored"));
    assert_eq!(response.durable_timeout_ms, None);
    assert_eq!(response.job_id, None);
    assert!(response.job.is_none());
}

#[cfg(windows)]
async fn assert_durable_job_finishes_ok(job_id: &str, expected_stdout: &str) {
    for _ in 0..100 {
        let status = shell_job_status(
            &ActRunShellStatusParams {
                job_id: job_id.to_owned(),
                tail_bytes: 4096,
            },
            None,
        )
        .unwrap_or_else(|error| panic!("status should read durable job state: {error}"));
        println!("readback=act_run_shell edge=durable_completion after=status:{status:?}");
        if status.job.status == "finalizing" {
            tokio::time::sleep(Duration::from_millis(25)).await;
            continue;
        }
        if !status.running {
            assert_eq!(status.job.status, "ok");
            assert_eq!(status.job.exit_code, Some(0));
            assert!(status.stdout_tail.contains(expected_stdout), "{status:?}");
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    panic!("background job {job_id} did not complete within the regression readback window");
}

#[cfg(windows)]
#[tokio::test]
async fn shell_durable_timeout_persists_budget_expired_code() {
    // Hermetic durable-job root so this test neither scans nor is perturbed
    // by jobs written by other tests running in parallel (#1509).
    let _root_guard = ShellJobRootGuard::new();
    let timeout_ms = 200;
    let args = vec![
        "-NoProfile".to_owned(),
        "-Command".to_owned(),
        "Start-Sleep -Milliseconds 2000".to_owned(),
    ];
    let auth_params = shell_params(
        "powershell.exe",
        args.iter().map(String::as_str).collect(),
        timeout_ms,
    );
    let authorization = authorize_run_shell(&shell_config_for(&auth_params), &auth_params)
        .unwrap_or_else(|error| panic!("durable timeout shell should authorize: {error}"));
    let started = start_authorized_shell_job(
        ActRunShellStartParams {
            command: "powershell.exe".to_owned(),
            args,
            working_dir: None,
            env: BTreeMap::new(),
            timeout_ms: Some(timeout_ms),
            job_id: None,
        },
        &authorization,
        None,
    )
    .unwrap_or_else(|error| panic!("durable timeout shell should start: {error}"));
    let job_id = started.job.job_id.clone();

    for _ in 0..100 {
        let status = shell_job_status(
            &ActRunShellStatusParams {
                job_id: job_id.clone(),
                tail_bytes: 4096,
            },
            None,
        )
        .unwrap_or_else(|error| panic!("durable timeout status should read: {error}"));
        println!("readback=act_run_shell_start edge=timeout_budget after=status:{status:?}");
        if status.job.status == "finalizing" || status.running {
            tokio::time::sleep(Duration::from_millis(25)).await;
            continue;
        }
        assert_eq!(status.job.status, "timed_out");
        assert!(status.job.timed_out);
        assert_eq!(
            status.job.error_code.as_deref(),
            Some(error_codes::ACTION_BUDGET_EXPIRED)
        );
        assert!(
            status
                .job
                .error_message
                .as_deref()
                .is_some_and(|message| message.contains("200 ms")),
            "{status:?}"
        );
        return;
    }

    panic!("durable timeout job {job_id} did not finish within the regression readback window");
}

// #1580 deterministic reproduction: under CPU starvation `tokio::time::timeout`
// delivers a child's own exit even when the deadline already fired (it polls
// the inner future before the timer), so a job that outran its cap was
// silently reported as a clean exit instead of timed_out. We reproduce the
// starvation exactly — a busy-loop task hogs the single runtime worker so the
// deadline timer cannot fire before the real child self-exits — and assert
// the budget is enforced from the measured wait, not the wait-vs-timer race.
#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn wait_shell_job_child_enforces_budget_when_starved_timer_misses_self_exit() {
    // Real child that self-exits well past the 80 ms cap (~200 ms sleep plus
    // interpreter startup). No mock: a genuine process on a genuine timer.
    // `started` is taken at spawn so the cap is measured from the process origin,
    // exactly as the production monitor does.
    let started = Instant::now();
    let mut child = TokioCommand::new("powershell.exe")
        .args(["-NoProfile", "-Command", "Start-Sleep -Milliseconds 200"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap_or_else(|error| panic!("spawn starvation child: {error}"));

    // Hog the single worker with a non-yielding busy loop so the 80 ms
    // deadline timer is starved past the child's self-exit; when the wait is
    // finally polled both the child exit and the blown deadline are ready and
    // `tokio::time::timeout` returns the child's `Ok(exit)`.
    let hog = tokio::spawn(async {
        let start = Instant::now();
        while start.elapsed() < Duration::from_millis(800) {
            std::hint::spin_loop();
        }
    });

    let (exit_code, timed_out, wait_error) =
        wait_shell_job_child(&mut child, Some(80), started).await;
    hog.await
        .unwrap_or_else(|error| panic!("hog task join: {error}"));

    println!(
        "readback=wait_shell_job_child edge=starved_self_exit after=exit_code:{exit_code:?} timed_out:{timed_out} wait_error:{wait_error:?}"
    );
    assert!(
        wait_error.is_none(),
        "no wait error expected: {wait_error:?}"
    );
    assert!(
        timed_out,
        "a job that ran ~200 ms under an 80 ms cap must be timed_out even when the starved timer let it self-exit (exit_code={exit_code:?})"
    );
}

// #1588 deterministic reproduction of the starvation shape the timer-miss test
// above does NOT cover: under heavy oversubscription the monitor task itself is
// dispatched only AFTER the child has already exited, so `child.wait()` resolves
// instantly and every wall clock the monitor samples is corrupted — reading ~0
// since wait-entry (false negative) or the whole starvation delay since spawn
// (false positive). Both directions must be resolved from the OS process runtime
// (exit - creation), which is independent of when this task ran. We drive both
// failure modes deterministically by letting the child fully exit, then waiting.
#[cfg(windows)]
#[tokio::test]
async fn wait_shell_job_child_classifies_budget_from_os_runtime_under_starvation() {
    // Case A (false-negative guard, #1588): the child genuinely outran its 40 ms
    // cap (~150 ms), but the monitor only reaches the wait ~400 ms after spawn,
    // so `child.wait()` resolves instantly. It must still be timed_out.
    let started = Instant::now();
    let mut over_budget = TokioCommand::new("powershell.exe")
        .args(["-NoProfile", "-Command", "Start-Sleep -Milliseconds 150"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap_or_else(|error| panic!("spawn over-budget child: {error}"));
    tokio::time::sleep(Duration::from_millis(400)).await;
    let (over_exit, over_timed_out, over_err) =
        wait_shell_job_child(&mut over_budget, Some(40), started).await;
    println!(
        "readback=wait_shell_job_child edge=over_budget_late_monitor after=exit_code:{over_exit:?} timed_out:{over_timed_out} wait_error:{over_err:?}"
    );
    assert!(over_err.is_none(), "no wait error expected: {over_err:?}");
    assert!(
        over_timed_out,
        "a child that ran ~150 ms under a 40 ms cap must be timed_out even when observed late (exit_code={over_exit:?})"
    );

    // Case B (false-positive guard, the regression a spawn-relative wall clock
    // introduces): a fast child (~tens of ms) finishes well within its 1000 ms
    // cap, yet the monitor is starved *past* the cap (1500 ms) before it observes
    // the exit. Only the OS runtime keeps this correctly "ok".
    let started = Instant::now();
    let mut within_budget = TokioCommand::new("cmd.exe")
        .args(["/c", "exit 0"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap_or_else(|error| panic!("spawn within-budget child: {error}"));
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let (within_exit, within_timed_out, within_err) =
        wait_shell_job_child(&mut within_budget, Some(1000), started).await;
    println!(
        "readback=wait_shell_job_child edge=within_budget_starved_monitor after=exit_code:{within_exit:?} timed_out:{within_timed_out} wait_error:{within_err:?}"
    );
    assert!(
        within_err.is_none(),
        "no wait error expected: {within_err:?}"
    );
    assert!(
        !within_timed_out,
        "a child that finished within its 1000 ms cap must not be flagged timed_out even when the monitor was starved 1500 ms (exit_code={within_exit:?})"
    );
}

// #1589 deterministic guard: an inline shell run that times out must terminate
// its process tree WITHOUT blocking the async runtime. On this single-threaded
// test runtime, a blocking termination (full-system sysinfo scan + taskkill
// spawns + a std::thread::sleep exit-wait) executed on the async worker freezes
// every task until it finishes — the mechanism that let concurrent scp timeouts
// stall one another for 90 s+. A heartbeat must keep ticking on schedule while a
// real inline timeout drives the termination path; a large gap means the
// blocking work ran on the async worker instead of the blocking pool.
#[cfg(windows)]
#[tokio::test]
async fn inline_shell_timeout_termination_keeps_async_runtime_responsive() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    let _root_guard = ShellJobRootGuard::new();

    let stop = Arc::new(AtomicBool::new(false));
    let stop_for_task = Arc::clone(&stop);
    let heartbeat = tokio::spawn(async move {
        let mut max_gap = Duration::ZERO;
        let mut ticks = 0_u32;
        let mut last = Instant::now();
        while !stop_for_task.load(Ordering::Relaxed) {
            tokio::time::sleep(Duration::from_millis(25)).await;
            let now = Instant::now();
            max_gap = max_gap.max(now.duration_since(last));
            last = now;
            ticks += 1;
        }
        (max_gap, ticks)
    });

    // Inline run whose child (10 s sleep) overruns its 300 ms cap, forcing the
    // process-tree termination path.
    let mut params = shell_params(
        "powershell.exe",
        vec!["-NoProfile", "-Command", "Start-Sleep -Seconds 10"],
        300,
    );
    params.execution_mode = ActRunShellExecutionMode::Inline;
    let config = shell_config_for(&params);
    let response = run_shell(&config, params)
        .await
        .unwrap_or_else(|error| panic!("inline timeout run should complete: {error}"));
    assert!(
        response.timed_out,
        "the overrunning inline run must be timed_out: {response:?}"
    );

    stop.store(true, Ordering::Relaxed);
    let (max_gap, ticks) = heartbeat
        .await
        .unwrap_or_else(|error| panic!("heartbeat join: {error}"));
    println!(
        "readback=inline_timeout_responsiveness after=timed_out:{} max_heartbeat_gap_ms:{} ticks:{ticks}",
        response.timed_out,
        max_gap.as_millis()
    );
    assert!(
        max_gap < Duration::from_millis(400),
        "the async runtime was frozen for {}ms during the shell termination; blocking process-tree work must run off the async worker (#1589)",
        max_gap.as_millis()
    );
}

#[cfg(windows)]
#[tokio::test]
async fn shell_session_cleanup_retains_live_durable_jobs() {
    // Hermetic durable-job root: the cleanup scan now enumerates only this
    // test's own jobs instead of every job in the process-wide shared root,
    // so a parallel test mutating that root can never flip this test's
    // exact-count assertions (#1509).
    let _root_guard = ShellJobRootGuard::new();
    let args = vec![
        "-NoProfile".to_owned(),
        "-Command".to_owned(),
        "Start-Sleep -Milliseconds 5000".to_owned(),
    ];
    let auth_params = shell_params(
        "powershell.exe",
        args.iter().map(String::as_str).collect(),
        30_000,
    );
    let authorization = authorize_run_shell(&shell_config_for(&auth_params), &auth_params)
        .unwrap_or_else(|error| panic!("durable cleanup shell should authorize: {error}"));
    // #1334: a unique session id per run so this test can never count a prior
    // run's (now dead-PID) durable job as live — combined with the PID-liveness
    // reconcile in cleanup_shell_jobs_for_session, the live count is exact.
    let session_label = format!("issue1334-cleanup-retain-{}", uuid::Uuid::new_v4());
    let context = shell_execution_context_for_session(&session_label)
        .unwrap_or_else(|error| panic!("shell context should build: {error}"));
    let started = start_authorized_shell_job(
        ActRunShellStartParams {
            command: "powershell.exe".to_owned(),
            args,
            working_dir: None,
            env: BTreeMap::new(),
            timeout_ms: Some(30_000),
            job_id: None,
        },
        &authorization,
        Some(&context),
    )
    .unwrap_or_else(|error| panic!("durable cleanup shell should start: {error}"));
    let job_id = started.job.job_id.clone();

    let foreign_session_label = format!("issue1461-cleanup-foreign-{}", uuid::Uuid::new_v4());
    let foreign_context = shell_execution_context_for_session(&foreign_session_label)
        .unwrap_or_else(|error| panic!("foreign shell context should build: {error}"));
    let foreign_started = start_authorized_shell_job(
        ActRunShellStartParams {
            command: "powershell.exe".to_owned(),
            args: vec![
                "-NoProfile".to_owned(),
                "-Command".to_owned(),
                "Start-Sleep -Milliseconds 5000".to_owned(),
            ],
            working_dir: None,
            env: BTreeMap::new(),
            timeout_ms: Some(30_000),
            job_id: None,
        },
        &authorization,
        Some(&foreign_context),
    )
    .unwrap_or_else(|error| panic!("foreign durable cleanup shell should start: {error}"));
    let foreign_job_id = foreign_started.job.job_id.clone();

    let cleanup = cleanup_shell_jobs_for_session(context.session_id(), "regression_stale")
        .unwrap_or_else(|error| panic!("session cleanup readback should succeed: {error}"));
    println!("readback=act_run_shell_session_cleanup edge=retain after={cleanup:?}");
    assert_eq!(cleanup.live_jobs_before, 1);
    assert_eq!(cleanup.retained_live_jobs, 1);
    // The hermetic root (#1509) means the scan sees exactly this test's two
    // jobs — the retained live one and the synthetic foreign one — and never
    // the process-wide pile of jobs left by other tests or prior runs.
    assert_eq!(
        cleanup.status_files_read, 2,
        "hermetic scan must read only this test's two jobs: {cleanup:?}"
    );
    assert_eq!(
        cleanup.skipped_foreign_jobs, 1,
        "cleanup should count exactly the synthetic foreign job: {cleanup:?}"
    );
    // No sibling directory should be mutating this test's private root, so
    // the concurrent-mutation reconciliation counter must stay at zero.
    assert_eq!(cleanup.skipped_concurrently_mutated, 0);
    // The only durable job under this unique session is genuinely alive, so
    // nothing should be reaped as a phantom (#1334).
    assert_eq!(cleanup.reaped_phantom_jobs, 0);
    assert_eq!(cleanup.termination_attempted, 0);
    assert_eq!(cleanup.failed, 0);
    assert!(cleanup.job_ids.contains(&job_id));
    assert!(!cleanup.job_ids.contains(&foreign_job_id));

    let retained = shell_job_status(
        &ActRunShellStatusParams {
            job_id: job_id.clone(),
            tail_bytes: 4096,
        },
        Some("fresh-session-after-cleanup"),
    )
    .unwrap_or_else(|error| panic!("fresh session should read retained durable job: {error}"));
    println!("readback=act_run_shell_status edge=retained after={retained:?}");
    assert!(retained.running);
    assert_eq!(retained.job.status, "running");
    assert!(!retained.job.cancel_requested);

    let cancelled = cancel_shell_job(
        &ActRunShellJobIdParams {
            job_id: job_id.clone(),
        },
        Some("fresh-session-after-cleanup"),
    )
    .unwrap_or_else(|error| panic!("fresh session should cancel retained durable job: {error}"));
    println!("readback=act_run_shell_cancel edge=retained_cleanup after={cancelled:?}");
    assert!(matches!(
        cancelled.status.job.status.as_str(),
        "cancelled" | "timed_out" | "exited_unobserved"
    ));

    let foreign_cancelled = cancel_shell_job(
        &ActRunShellJobIdParams {
            job_id: foreign_job_id.clone(),
        },
        Some(foreign_context.session_id()),
    )
    .unwrap_or_else(|error| panic!("foreign owner should cancel retained job: {error}"));
    println!("readback=act_run_shell_cancel edge=foreign_cleanup after={foreign_cancelled:?}");
    assert!(matches!(
        foreign_cancelled.status.job.status.as_str(),
        "cancelled" | "timed_out" | "exited_unobserved"
    ));
}

#[test]
fn launch_rejects_zero_timeout_and_accepts_large_caller_budget() {
    let zero = launch_params("notepad.exe", Vec::new(), 0);
    let error = match validate_launch_params(&zero) {
        Ok(()) => panic!("zero timeout should reject"),
        Err(error) => error,
    };

    assert_eq!(
        error
            .data
            .as_ref()
            .and_then(|data| data.get("code"))
            .and_then(|code| code.as_str()),
        Some(error_codes::TOOL_PARAMS_INVALID)
    );
    assert!(error.message.contains("timeout_ms must be >= 1"));

    let large = launch_params("notepad.exe", Vec::new(), 1_200_000);
    validate_launch_params(&large)
        .unwrap_or_else(|error| panic!("large explicit caller timeout should accept: {error}"));
}

#[test]
fn launch_process_history_row_records_spawn_without_env_values() {
    let mut params = launch_params("notepad.exe", vec!["C:\\tmp\\launch.txt"], 10_000);
    params.env.insert(
        "SYNAPSE_LAUNCH_SECRET".to_owned(),
        "do-not-store".to_owned(),
    );
    let response = ActLaunchResponse {
        pid: 1234,
        hwnd: Some(5678),
        window_owner_pid: Some(1234),
        reused_existing_window: false,
        matched_title: Some("launch.txt - Notepad".to_owned()),
        launched_at: "2026-05-31T20:00:00Z".to_owned(),
        reason: None,
        cdp_debug_port: None,
        cdp_endpoint: None,
        cdp_user_data_dir: None,
        cdp_verified_url: None,
        cdp_verified_title: None,
        desktop: None,
    };

    let row = launch_process_history_row(&params, &response)
        .unwrap_or_else(|error| panic!("process history row should encode: {error}"));
    let value: serde_json::Value = serde_json::from_slice(&row)
        .unwrap_or_else(|error| panic!("process history row should decode: {error}"));

    assert_eq!(value["tool"], "act_launch");
    assert_eq!(value["pid"], 1234);
    assert_eq!(value["hwnd"], 5678);
    assert_eq!(value["matched_title"], "launch.txt - Notepad");
    assert_eq!(value["env_keys"], json!(["SYNAPSE_LAUNCH_SECRET"]));
    assert_eq!(value["cdp_debug"], serde_json::Value::Null);
    assert_eq!(value["cdp_debug_port"], serde_json::Value::Null);
    assert_eq!(value["cdp_endpoint"], serde_json::Value::Null);
    assert_eq!(value["cdp_user_data_dir"], serde_json::Value::Null);
    assert_eq!(value["cdp_verified_url"], serde_json::Value::Null);
    assert_eq!(value["cdp_verified_title"], serde_json::Value::Null);
    assert!(!String::from_utf8_lossy(&row).contains("do-not-store"));
    assert!(String::from_utf8_lossy(&launch_process_history_row_key(&response)).contains("1234"));
}

#[test]
fn launch_process_history_row_records_cdp_launch_fields() {
    let mut params = launch_params("chrome.exe", vec!["https://example.test"], 10_000);
    params.cdp_debug = Some(true);
    let response = ActLaunchResponse {
        pid: 2222,
        hwnd: Some(3333),
        window_owner_pid: Some(2222),
        reused_existing_window: false,
        matched_title: Some("Synthetic CDP Page - Google Chrome".to_owned()),
        launched_at: "2026-06-03T23:00:00Z".to_owned(),
        reason: None,
        cdp_debug_port: Some(45678),
        cdp_endpoint: Some("http://127.0.0.1:45678".to_owned()),
        cdp_user_data_dir: Some("C:\\Temp\\synapse-cdp-profiles\\synthetic".to_owned()),
        cdp_verified_url: Some("https://example.test/".to_owned()),
        cdp_verified_title: Some("Synthetic CDP Page".to_owned()),
        desktop: None,
    };

    let row = launch_process_history_row(&params, &response)
        .unwrap_or_else(|error| panic!("process history row should encode: {error}"));
    let value: serde_json::Value = serde_json::from_slice(&row)
        .unwrap_or_else(|error| panic!("process history row should decode: {error}"));

    println!(
        "readback=act_launch_history_cdp before=port:{:?} after=row_port:{} endpoint:{}",
        response.cdp_debug_port, value["cdp_debug_port"], value["cdp_endpoint"]
    );
    assert_eq!(value["cdp_debug"], true);
    assert_eq!(value["cdp_debug_port"], 45678);
    assert_eq!(value["cdp_endpoint"], "http://127.0.0.1:45678");
    assert_eq!(
        value["cdp_user_data_dir"],
        "C:\\Temp\\synapse-cdp-profiles\\synthetic"
    );
    assert_eq!(value["cdp_verified_url"], "https://example.test/");
    assert_eq!(value["cdp_verified_title"], "Synthetic CDP Page");
}

#[test]
fn launch_console_targets_request_real_console_windows() {
    for target in [
        "cmd",
        "cmd.exe",
        "C:\\Windows\\System32\\cmd.exe",
        "powershell",
        "powershell.exe",
        "C:\\Program Files\\PowerShell\\7\\pwsh.exe",
    ] {
        assert!(
            launch_target_needs_new_console(target),
            "{target} should request CREATE_NEW_CONSOLE on Windows"
        );
    }

    for target in ["notepad.exe", "wt.exe", "WindowsTerminal.exe"] {
        assert!(
            !launch_target_needs_new_console(target),
            "{target} should use normal GUI launch stdio handling"
        );
    }
}

#[cfg(windows)]
#[test]
fn windows_launch_startup_state_is_non_activating_for_gui_and_hidden_for_console() {
    let gui = launch_params("notepad.exe", Vec::new(), 10_000);
    let console = launch_params("cmd.exe", vec!["/c", "exit"], 10_000);

    let gui_show = windows_launch_show_window(&gui);
    let console_show = windows_launch_show_window(&console);

    println!(
        "readback=act_launch_startup_show_state before=gui:notepad.exe,console:cmd.exe after=gui:{gui_show} console:{console_show}"
    );
    assert_eq!(gui_show, SW_SHOWNOACTIVATE);
    assert_eq!(console_show, SW_HIDE);
}

#[cfg(windows)]
#[test]
fn windows_launch_creation_flags_do_not_hide_gui_targets() {
    use windows::Win32::System::Threading::{
        CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT,
    };

    let gui = launch_params("notepad.exe", Vec::new(), 10_000);
    let console = launch_params("cmd.exe", vec!["/c", "exit"], 10_000);

    let gui_flags = windows_launch_creation_flags(&gui);
    let console_flags = windows_launch_creation_flags(&console);

    println!(
        "readback=act_launch_creation_flags before=gui:notepad.exe,console:cmd.exe after=gui:0x{:x} console:0x{:x}",
        gui_flags.0, console_flags.0
    );
    assert_ne!(gui_flags.0 & CREATE_UNICODE_ENVIRONMENT.0, 0);
    assert_eq!(gui_flags.0 & CREATE_NO_WINDOW.0, 0);
    assert_eq!(gui_flags.0 & CREATE_NEW_PROCESS_GROUP.0, 0);

    assert_ne!(console_flags.0 & CREATE_UNICODE_ENVIRONMENT.0, 0);
    assert_ne!(console_flags.0 & CREATE_NO_WINDOW.0, 0);
    assert_ne!(console_flags.0 & CREATE_NEW_PROCESS_GROUP.0, 0);
}

#[cfg(windows)]
#[test]
fn hidden_desktop_enum_missing_or_exhausted_is_empty_readback() {
    use windows::Win32::Foundation::{
        ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND, ERROR_NO_MORE_FILES,
    };
    use windows::core::Error;

    let file_not_found = Error::from_hresult(ERROR_FILE_NOT_FOUND.to_hresult());
    let no_more_files = Error::from_hresult(ERROR_NO_MORE_FILES.to_hresult());
    let access_denied = Error::from_hresult(ERROR_ACCESS_DENIED.to_hresult());

    println!(
        "readback=act_launch_desktop_enum_error before=file_not_found,no_more_files,access_denied after=empty:{},{} fail:{}",
        desktop_window_enum_error_means_empty(&file_not_found),
        desktop_window_enum_error_means_empty(&no_more_files),
        desktop_window_enum_error_means_empty(&access_denied)
    );
    assert!(desktop_window_enum_error_means_empty(&file_not_found));
    assert!(desktop_window_enum_error_means_empty(&no_more_files));
    assert!(!desktop_window_enum_error_means_empty(&access_denied));
}

#[test]
fn shell_idempotency_replays_matching_completed_row() {
    let mut params = shell_params("cmd.exe", vec!["/c", "echo replay"], 30_000);
    params.idempotency_key = Some("issue-606-replay".to_owned());
    let authorization = authorize_run_shell(&shell_config_for(&params), &params)
        .unwrap_or_else(|error| panic!("authorized shell params: {error}"));
    let response = ActRunShellResponse {
        exit_code: Some(0),
        stdout: "replay\r\n".to_owned(),
        stderr: String::new(),
        duration_ms: 12,
        timed_out: false,
        error_code: None,
        error_message: None,
        stdout_truncated: false,
        stderr_truncated: false,
        session_id: Some("session-a".to_owned()),
        effective_working_dir: Some("C:\\code\\Synapse".to_owned()),
        backgrounded: false,
        background_reason: None,
        inline_await_limit_ms: None,
        inline_client_call_budget_ms: None,
        requested_execution_mode: Some(ActRunShellExecutionMode::Auto),
        effective_execution_mode: Some(ActRunShellExecutionMode::Inline),
        durable_timeout_ms: None,
        job_id: None,
        job: None,
    };
    let row =
        run_shell_idempotency_completed_row(&params, &authorization, &response, Some("session-a"))
            .unwrap_or_else(|error| panic!("completed idempotency row should encode: {error}"));

    let replay = run_shell_idempotency_replay(&params, &row, Some("session-a"))
        .unwrap_or_else(|error| panic!("matching idempotency row should replay: {error}"));

    assert_eq!(replay.stdout, "replay\r\n");
    assert_eq!(replay.exit_code, Some(0));
}

#[test]
fn shell_idempotency_rejects_conflicting_request_reuse() {
    let mut first = shell_params("cmd.exe", vec!["/c", "echo first"], 30_000);
    first.idempotency_key = Some("issue-606-conflict".to_owned());
    let authorization = authorize_run_shell(&shell_config_for(&first), &first)
        .unwrap_or_else(|error| panic!("first shell params should authorize: {error}"));
    let response = ActRunShellResponse {
        exit_code: Some(0),
        stdout: "first\r\n".to_owned(),
        stderr: String::new(),
        duration_ms: 10,
        timed_out: false,
        error_code: None,
        error_message: None,
        stdout_truncated: false,
        stderr_truncated: false,
        session_id: Some("session-a".to_owned()),
        effective_working_dir: Some("C:\\code\\Synapse".to_owned()),
        backgrounded: false,
        background_reason: None,
        inline_await_limit_ms: None,
        inline_client_call_budget_ms: None,
        requested_execution_mode: Some(ActRunShellExecutionMode::Auto),
        effective_execution_mode: Some(ActRunShellExecutionMode::Inline),
        durable_timeout_ms: None,
        job_id: None,
        job: None,
    };
    let row =
        run_shell_idempotency_completed_row(&first, &authorization, &response, Some("session-a"))
            .unwrap_or_else(|error| panic!("completed idempotency row should encode: {error}"));
    let mut second = shell_params("cmd.exe", vec!["/c", "echo second"], 30_000);
    second.idempotency_key = first.idempotency_key.clone();

    let error = match run_shell_idempotency_replay(&second, &row, Some("session-a")) {
        Ok(replay) => panic!("conflicting idempotency reuse should reject, got {replay:?}"),
        Err(error) => error,
    };

    assert_eq!(
        error
            .data
            .as_ref()
            .and_then(|data| data.get("reason"))
            .and_then(|reason| reason.as_str()),
        Some("idempotency_key_conflict")
    );
}

#[test]
fn shell_idempotency_key_is_partitioned_by_session() {
    let mut params = shell_params("cmd.exe", vec!["/c", "echo owner"], 30_000);
    params.idempotency_key = Some("issue-802-owner".to_owned());

    let session_a = run_shell_idempotency_row_key(&params, Some("session-a"))
        .unwrap_or_else(|error| panic!("session-a key should encode: {error}"));
    let session_b = run_shell_idempotency_row_key(&params, Some("session-b"))
        .unwrap_or_else(|error| panic!("session-b key should encode: {error}"));

    assert_ne!(session_a, session_b);
}
