use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    path::{Path, PathBuf},
};

use rmcp::{ErrorData, model::ErrorCode};
use serde_json::json;
use synapse_core::{ForegroundContext, Profile, ProfileId, error_codes};
use synapse_profiles::{ForegroundWindow, ProfileRuntime};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

const KEY_LOCAL_WORLD_ONLY: &str = "supported_use.local_world_only";
const KEY_APPROVED_WORLDS: &str = "supported_use.approved_worlds";
const KEY_REMOTE_SERVER_ALLOWED: &str = "supported_use.remote_server_allowed";
const KEY_LIVE_SERVER_ALLOWED: &str = "supported_use.live_server_allowed";
const KEY_OPERATOR_ATTENDED_REQUIRED: &str = "supported_use.operator_attended_required";
const KEY_OPERATOR_OWNED_CHARACTER_REQUIRED: &str =
    "supported_use.operator_owned_character_required";
const KEY_FOREGROUND_ONLY: &str = "supported_use.foreground_only";
const KEY_NO_MEMORY_OR_PROTOCOL_HOOKS: &str = "supported_use.no_memory_or_protocol_hooks";
const KEY_NO_UNATTENDED_LOOPS: &str = "supported_use.no_unattended_loops";
const KEY_NO_SOCIAL_OR_ECONOMY_AUTOMATION: &str = "supported_use.no_social_or_economy_automation";
const KEY_NO_UNATTENDED_SCALED_OPERATION: &str = "supported_use.no_unattended_scaled_operation";
const KEY_NO_ACCOUNT_OR_BILLING_AUTOMATION: &str = "supported_use.no_account_or_billing_automation";
const KEY_NO_PVP_GROUP_GUILD_RAID_AUTOMATION: &str =
    "supported_use.no_pvp_group_guild_raid_automation";
const KEY_NO_DESTRUCTIVE_UI_AUTOMATION: &str = "supported_use.no_destructive_ui_automation";
const KEY_LAUNCH_TARGET: &str = "launch.target";
const KEY_LAUNCH_WORLD: &str = "launch.world";
const KEY_LAUNCH_LOGFILE: &str = "launch.logfile";
const KEY_BENCHMARK_GAMEID: &str = "benchmark_world_gameid";
const KEY_RUNTIME_LIVE_SERVER_EXE: &str = "runtime.live_server.exe";
const KEY_RUNTIME_LIVE_SERVER_NAME: &str = "runtime.live_server.name";

/// Opt-in enforcement of the legacy game-target `supported_use` policy.
///
/// Synapse is general-purpose local Windows computer-control infrastructure;
/// by default every profiled foreground app is actionable and this
/// world/server gate does nothing. Set this env truthy to restore the
/// historical benchmark/live-server gating (used only by the bundled game
/// profiles). Functional safety (operator panic hotkey, release-all on panic,
/// rate limits, foreground stabilization) is independent of this flag and
/// always active.
const ENFORCE_SUPPORTED_USE_ENV: &str = "SYNAPSE_ENFORCE_SUPPORTED_USE";

fn supported_use_enforced() -> bool {
    env::var_os(ENFORCE_SUPPORTED_USE_ENV).is_some_and(|raw| {
        matches!(
            raw.to_string_lossy().trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "y" | "on"
        )
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct SupportedTargetState {
    pub profile_id: ProfileId,
    pub foreground_pid: u32,
    pub foreground_process_path: String,
    pub process_command_line: Vec<String>,
    pub world_path: PathBuf,
    pub world_name: String,
    pub gameid: String,
    pub logfile_path: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct TargetPolicyDenial {
    reason: &'static str,
    detail: String,
    profile_id: ProfileId,
    foreground_pid: u32,
    foreground_process_path: String,
    evidence: DenialEvidence,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct DenialEvidence {
    world_path: Option<PathBuf>,
    logfile_path: Option<PathBuf>,
    observed_world_name: Option<String>,
    observed_gameid: Option<String>,
    process_command_line: Option<Vec<String>>,
}

type TargetPolicyResult<T> = Result<T, Box<TargetPolicyDenial>>;

#[derive(Clone, Debug, Eq, PartialEq)]
struct ForegroundProcessState {
    command_line: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct TargetStateReadback {
    process_state: ForegroundProcessState,
    world_path: PathBuf,
    logfile_path: PathBuf,
    world_name: String,
    gameid: String,
}

pub(super) fn ensure_supported_use_allows(
    runtime: &ProfileRuntime,
    foreground: &ForegroundContext,
    tool: &'static str,
) -> Result<(), ErrorData> {
    if !supported_use_enforced() {
        // Default posture: general Windows computer-control. The legacy
        // game world/server gate is opt-in via SYNAPSE_ENFORCE_SUPPORTED_USE.
        return Ok(());
    }
    let observed_profile_id = runtime
        .resolve_foreground(&foreground_window(foreground))
        .map_err(|error| target_policy_internal_error(tool, &error.to_string()))?
        .map(|resolution| resolution.profile_id);
    let active_profile_id = runtime
        .active_profile_id()
        .map_err(|error| target_policy_internal_error(tool, &error.to_string()))?;
    let active_profile = match active_profile_id {
        Some(profile_id) => runtime
            .profile(&profile_id)
            .map_err(|error| target_policy_internal_error(tool, &error.to_string()))?,
        None => None,
    };
    let profile = match active_profile {
        Some(profile) if has_supported_use_policy(&profile) => profile,
        _ => {
            let Some(profile_id) = observed_profile_id.clone() else {
                return Ok(());
            };
            let Some(profile) = runtime
                .profile(&profile_id)
                .map_err(|error| target_policy_internal_error(tool, &error.to_string()))?
            else {
                return Ok(());
            };
            if !has_supported_use_policy(&profile) {
                return Ok(());
            }
            profile
        }
    };
    if observed_profile_id.as_deref() != Some(profile.id.as_str()) {
        return Err(target_policy_denied_error(
            tool,
            Box::new(TargetPolicyDenial {
                reason: "profile_not_foreground",
                detail: format!(
                    "profile {} has supported_use policy but current foreground did not resolve to that profile",
                    profile.id
                ),
                profile_id: profile.id,
                foreground_pid: foreground.pid,
                foreground_process_path: foreground.process_path.clone(),
                evidence: DenialEvidence::default(),
            }),
        ));
    }

    match evaluate_supported_use_with_optional_process_for_tool(&profile, foreground, tool, None) {
        Ok(state) => {
            tracing::info!(
                code = "SAFETY_PROFILE_TARGET_ALLOWED",
                tool,
                profile_id = %state.profile_id,
                foreground_pid = state.foreground_pid,
                foreground_process_path = %state.foreground_process_path,
                process_command_line = ?state.process_command_line,
                world_path = %state.world_path.display(),
                world_name = %state.world_name,
                gameid = %state.gameid,
                logfile_path = %state.logfile_path.display(),
                "supported_use target policy allowed action dispatch"
            );
            Ok(())
        }
        Err(denial) => Err(target_policy_denied_error(tool, denial)),
    }
}

fn evaluate_supported_use_with_optional_process_for_tool(
    profile: &Profile,
    foreground: &ForegroundContext,
    tool: &'static str,
    process_state: Option<ForegroundProcessState>,
) -> TargetPolicyResult<SupportedTargetState> {
    let local_world_only = metadata_bool(&profile.metadata, KEY_LOCAL_WORLD_ONLY);
    let remote_allowed = metadata_bool(&profile.metadata, KEY_REMOTE_SERVER_ALLOWED);
    let live_server_allowed = metadata_bool(&profile.metadata, KEY_LIVE_SERVER_ALLOWED);
    if live_server_allowed {
        return evaluate_operator_attended_live_server(profile, foreground, tool);
    }
    if !local_world_only && remote_allowed {
        return Ok(empty_supported_target_state(profile, foreground));
    }

    let launch_target = optional_expanded_path(&profile.metadata, KEY_LAUNCH_TARGET);
    if let Some(target) = &launch_target
        && !same_path_text(target, Path::new(&foreground.process_path))
    {
        return Err(denial(
            profile,
            foreground,
            "process_path_mismatch",
            format!(
                "foreground process path {} does not match profile launch target {}",
                foreground.process_path,
                target.display()
            ),
            DenialEvidence::default(),
        ));
    }
    let readback = read_target_state(profile, foreground, process_state)?;

    let approved_worlds = approved_worlds(&profile.metadata);
    if approved_worlds.is_empty() {
        return Err(denial(
            profile,
            foreground,
            "approved_worlds_missing",
            "supported_use.approved_worlds is empty".to_owned(),
            denial_evidence(&readback),
        ));
    }
    if !approved_worlds.contains(&readback.world_name) {
        return Err(denial(
            profile,
            foreground,
            "world_not_approved",
            format!(
                "world {:?} is not in supported_use.approved_worlds",
                readback.world_name
            ),
            denial_evidence(&readback),
        ));
    }
    if let Some(expected_gameid) = metadata_trimmed(&profile.metadata, KEY_BENCHMARK_GAMEID)
        && expected_gameid != readback.gameid
    {
        return Err(denial(
            profile,
            foreground,
            "gameid_mismatch",
            format!(
                "world gameid {:?} does not match expected {expected_gameid:?}",
                readback.gameid
            ),
            denial_evidence(&readback),
        ));
    }

    let latest_session = read_latest_log_session(
        profile,
        foreground,
        &readback.world_path,
        &readback.logfile_path,
    )?;
    if local_world_only && !log_session_mentions_world(&latest_session, &readback.world_path) {
        return Err(denial(
            profile,
            foreground,
            "local_world_log_missing",
            "latest Luanti log session does not prove the configured local world".to_owned(),
            denial_evidence(&readback),
        ));
    }
    if !remote_allowed && !log_session_mentions_gameid(&latest_session, &readback.gameid) {
        return Err(denial(
            profile,
            foreground,
            "remote_or_unapproved_session",
            "latest Luanti log session does not prove a local approved game server".to_owned(),
            denial_evidence(&readback),
        ));
    }

    Ok(SupportedTargetState {
        profile_id: profile.id.clone(),
        foreground_pid: foreground.pid,
        foreground_process_path: foreground.process_path.clone(),
        process_command_line: readback.process_state.command_line,
        world_path: readback.world_path,
        world_name: readback.world_name,
        gameid: readback.gameid,
        logfile_path: readback.logfile_path,
    })
}

fn evaluate_operator_attended_live_server(
    profile: &Profile,
    foreground: &ForegroundContext,
    tool: &'static str,
) -> TargetPolicyResult<SupportedTargetState> {
    require_metadata_bool(
        profile,
        foreground,
        KEY_OPERATOR_ATTENDED_REQUIRED,
        "operator_attended_required_missing",
    )?;
    require_metadata_bool(
        profile,
        foreground,
        KEY_OPERATOR_OWNED_CHARACTER_REQUIRED,
        "operator_owned_character_required_missing",
    )?;
    require_metadata_bool(
        profile,
        foreground,
        KEY_FOREGROUND_ONLY,
        "foreground_only_required_missing",
    )?;
    require_metadata_bool(
        profile,
        foreground,
        KEY_NO_MEMORY_OR_PROTOCOL_HOOKS,
        "no_memory_or_protocol_hooks_missing",
    )?;
    require_metadata_bool(
        profile,
        foreground,
        KEY_NO_UNATTENDED_LOOPS,
        "no_unattended_loops_missing",
    )?;
    require_metadata_bool(
        profile,
        foreground,
        KEY_NO_SOCIAL_OR_ECONOMY_AUTOMATION,
        "no_social_or_economy_automation_missing",
    )?;
    require_metadata_bool(
        profile,
        foreground,
        KEY_NO_UNATTENDED_SCALED_OPERATION,
        "no_unattended_scaled_operation_missing",
    )?;
    require_metadata_bool(
        profile,
        foreground,
        KEY_NO_ACCOUNT_OR_BILLING_AUTOMATION,
        "no_account_or_billing_automation_missing",
    )?;
    require_metadata_bool(
        profile,
        foreground,
        KEY_NO_PVP_GROUP_GUILD_RAID_AUTOMATION,
        "no_pvp_group_guild_raid_automation_missing",
    )?;
    require_metadata_bool(
        profile,
        foreground,
        KEY_NO_DESTRUCTIVE_UI_AUTOMATION,
        "no_destructive_ui_automation_missing",
    )?;
    if let Some(detail) = live_server_denied_tool_detail(tool) {
        return Err(denial(
            profile,
            foreground,
            "live_server_tool_not_foreground_input",
            detail.to_owned(),
            DenialEvidence::default(),
        ));
    }

    if let Some(expected_exe) =
        optional_expanded_path(&profile.metadata, KEY_RUNTIME_LIVE_SERVER_EXE)
        && !same_path_text(&expected_exe, Path::new(&foreground.process_path))
    {
        return Err(denial(
            profile,
            foreground,
            "process_path_mismatch",
            format!(
                "foreground process path {} does not match profile runtime target {}",
                foreground.process_path,
                expected_exe.display()
            ),
            DenialEvidence::default(),
        ));
    }
    if metadata_trimmed(&profile.metadata, KEY_RUNTIME_LIVE_SERVER_NAME).is_none() {
        return Err(denial(
            profile,
            foreground,
            "live_server_name_missing",
            format!("{KEY_RUNTIME_LIVE_SERVER_NAME} metadata is required"),
            DenialEvidence::default(),
        ));
    }

    Ok(empty_supported_target_state(profile, foreground))
}

fn live_server_denied_tool_detail(tool: &str) -> Option<&'static str> {
    match tool {
        "act_type" => Some(
            "act_type is denied for operator-attended live-server profiles because text entry can drive chat, social, economy, account, or command surfaces",
        ),
        "act_clipboard" => Some(
            "mutating act_clipboard is denied for operator-attended live-server profiles because clipboard text can be pasted into chat, social, economy, account, or command surfaces",
        ),
        "act_combo" => Some(
            "act_combo is denied for operator-attended live-server profiles; use individually prompted foreground actions with source-of-truth readback",
        ),
        "act_run_shell" => Some(
            "act_run_shell is denied for operator-attended live-server profiles because it is not foreground game input",
        ),
        "act_launch" => Some(
            "act_launch is denied for operator-attended live-server profiles because live gameplay must use the already foreground game window",
        ),
        "reflex_register" => Some(
            "reflex_register is denied for operator-attended live-server profiles because background reflex dispatch is not supervised foreground input",
        ),
        _ => None,
    }
}

fn require_metadata_bool(
    profile: &Profile,
    foreground: &ForegroundContext,
    key: &'static str,
    reason: &'static str,
) -> TargetPolicyResult<()> {
    if metadata_bool(&profile.metadata, key) {
        return Ok(());
    }
    Err(denial(
        profile,
        foreground,
        reason,
        format!("{key}=true metadata is required"),
        DenialEvidence::default(),
    ))
}

fn empty_supported_target_state(
    profile: &Profile,
    foreground: &ForegroundContext,
) -> SupportedTargetState {
    SupportedTargetState {
        profile_id: profile.id.clone(),
        foreground_pid: foreground.pid,
        foreground_process_path: foreground.process_path.clone(),
        process_command_line: Vec::new(),
        world_path: PathBuf::new(),
        world_name: String::new(),
        gameid: String::new(),
        logfile_path: PathBuf::new(),
    }
}

fn read_target_state(
    profile: &Profile,
    foreground: &ForegroundContext,
    process_state: Option<ForegroundProcessState>,
) -> TargetPolicyResult<TargetStateReadback> {
    let process_state = match process_state {
        Some(process_state) => process_state,
        None => read_foreground_process_state(profile, foreground)?,
    };
    let world_path = required_expanded_path(profile, foreground, KEY_LAUNCH_WORLD)?;
    let logfile_path = required_expanded_path(profile, foreground, KEY_LAUNCH_LOGFILE)?;
    ensure_process_arg_path(
        profile,
        foreground,
        &process_state,
        "--world",
        &world_path,
        "process_world_arg_mismatch",
    )?;
    ensure_process_arg_path(
        profile,
        foreground,
        &process_state,
        "--logfile",
        &logfile_path,
        "process_logfile_arg_mismatch",
    )?;
    let world_mt_path = world_path.join("world.mt");
    let world = read_world_metadata(profile, foreground, &world_path, &world_mt_path)?;
    let world_name = world_value(profile, foreground, &world_path, &world, "world_name")?;
    let gameid = world_value(profile, foreground, &world_path, &world, "gameid")?;
    ensure_process_arg_value(
        profile,
        foreground,
        &process_state,
        "--gameid",
        &gameid,
        "process_gameid_arg_mismatch",
    )?;
    Ok(TargetStateReadback {
        process_state,
        world_path,
        logfile_path,
        world_name,
        gameid,
    })
}

fn denial_evidence(readback: &TargetStateReadback) -> DenialEvidence {
    DenialEvidence {
        world_path: Some(readback.world_path.clone()),
        logfile_path: Some(readback.logfile_path.clone()),
        observed_world_name: Some(readback.world_name.clone()),
        observed_gameid: Some(readback.gameid.clone()),
        process_command_line: Some(readback.process_state.command_line.clone()),
    }
}

fn has_supported_use_policy(profile: &Profile) -> bool {
    profile
        .metadata
        .keys()
        .any(|key| key.starts_with("supported_use."))
}

fn foreground_window(foreground: &ForegroundContext) -> ForegroundWindow {
    ForegroundWindow {
        exe: non_empty(&foreground.process_name),
        title: non_empty(&foreground.window_title),
        steam_appid: foreground.steam_appid,
        window_class: None,
    }
}

fn non_empty(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

fn metadata_bool(metadata: &BTreeMap<String, String>, key: &str) -> bool {
    metadata.get(key).is_some_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "y" | "on"
        )
    })
}

fn metadata_trimmed<'a>(metadata: &'a BTreeMap<String, String>, key: &str) -> Option<&'a str> {
    metadata
        .get(key)
        .map(String::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn optional_expanded_path(metadata: &BTreeMap<String, String>, key: &str) -> Option<PathBuf> {
    metadata_trimmed(metadata, key).map(expand_env_path)
}

fn required_expanded_path(
    profile: &Profile,
    foreground: &ForegroundContext,
    key: &'static str,
) -> TargetPolicyResult<PathBuf> {
    metadata_trimmed(&profile.metadata, key)
        .map(expand_env_path)
        .ok_or_else(|| {
            denial(
                profile,
                foreground,
                "target_state_metadata_missing",
                format!("{key} metadata is required by supported_use policy"),
                DenialEvidence::default(),
            )
        })
}

fn expand_env_path(raw: &str) -> PathBuf {
    PathBuf::from(expand_percent_env(raw))
}

fn expand_percent_env(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(start) = rest.find('%') {
        out.push_str(&rest[..start]);
        let after_start = &rest[start + 1..];
        let Some(end) = after_start.find('%') else {
            out.push('%');
            out.push_str(after_start);
            return out;
        };
        let name = &after_start[..end];
        if name.is_empty() {
            out.push_str("%%");
        } else if let Ok(value) = env::var(name) {
            out.push_str(&value);
        } else {
            out.push('%');
            out.push_str(name);
            out.push('%');
        }
        rest = &after_start[end + 1..];
    }
    out.push_str(rest);
    out
}

fn same_path_text(left: &Path, right: &Path) -> bool {
    comparable_path(left) == comparable_path(right)
}

fn comparable_path(path: &Path) -> String {
    path.to_string_lossy()
        .replace('/', "\\")
        .trim_end_matches('\\')
        .to_ascii_lowercase()
}

fn read_foreground_process_state(
    profile: &Profile,
    foreground: &ForegroundContext,
) -> TargetPolicyResult<ForegroundProcessState> {
    let pid = Pid::from_u32(foreground.pid);
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid]),
        true,
        ProcessRefreshKind::nothing()
            .with_cmd(UpdateKind::Always)
            .with_exe(UpdateKind::Always)
            .without_tasks(),
    );
    let Some(process) = system.process(pid) else {
        return Err(denial(
            profile,
            foreground,
            "process_not_found",
            format!(
                "foreground pid {} was not present in the process table",
                foreground.pid
            ),
            DenialEvidence::default(),
        ));
    };
    let command_line = process
        .cmd()
        .iter()
        .map(|part| part.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    if command_line.is_empty() {
        return Err(denial(
            profile,
            foreground,
            "process_command_line_unreadable",
            format!(
                "could not read command line for foreground pid {}",
                foreground.pid
            ),
            DenialEvidence::default(),
        ));
    }
    Ok(ForegroundProcessState { command_line })
}

fn ensure_process_arg_path(
    profile: &Profile,
    foreground: &ForegroundContext,
    process_state: &ForegroundProcessState,
    flag: &'static str,
    expected_path: &Path,
    reason: &'static str,
) -> TargetPolicyResult<()> {
    if command_line_has_path_arg(&process_state.command_line, flag, expected_path) {
        return Ok(());
    }
    Err(denial(
        profile,
        foreground,
        reason,
        format!(
            "foreground process command line is missing {flag} {}",
            expected_path.display()
        ),
        DenialEvidence {
            process_command_line: Some(process_state.command_line.clone()),
            ..DenialEvidence::default()
        },
    ))
}

fn ensure_process_arg_value(
    profile: &Profile,
    foreground: &ForegroundContext,
    process_state: &ForegroundProcessState,
    flag: &'static str,
    expected_value: &str,
    reason: &'static str,
) -> TargetPolicyResult<()> {
    if command_line_has_value_arg(&process_state.command_line, flag, expected_value) {
        return Ok(());
    }
    Err(denial(
        profile,
        foreground,
        reason,
        format!("foreground process command line is missing {flag} {expected_value}"),
        DenialEvidence {
            process_command_line: Some(process_state.command_line.clone()),
            ..DenialEvidence::default()
        },
    ))
}

fn command_line_has_path_arg(command_line: &[String], flag: &str, expected_path: &Path) -> bool {
    command_line_values(command_line, flag)
        .iter()
        .any(|value| same_path_text(Path::new(value), expected_path))
}

fn command_line_has_value_arg(command_line: &[String], flag: &str, expected_value: &str) -> bool {
    command_line_values(command_line, flag)
        .iter()
        .any(|value| value == expected_value)
}

fn command_line_values(command_line: &[String], flag: &str) -> Vec<String> {
    let mut values = Vec::new();
    for (index, arg) in command_line.iter().enumerate() {
        if arg.eq_ignore_ascii_case(flag) {
            if let Some(value) = command_line.get(index + 1) {
                values.push(value.clone());
            }
            continue;
        }
        let Some((arg_flag, value)) = arg.split_once('=') else {
            continue;
        };
        if arg_flag.eq_ignore_ascii_case(flag) {
            values.push(value.to_owned());
        }
    }
    values
}

fn read_world_metadata(
    profile: &Profile,
    foreground: &ForegroundContext,
    world_path: &Path,
    world_mt_path: &Path,
) -> TargetPolicyResult<BTreeMap<String, String>> {
    let raw = fs::read_to_string(world_mt_path).map_err(|error| {
        denial(
            profile,
            foreground,
            "world_metadata_unreadable",
            format!("could not read {}: {error}", world_mt_path.display()),
            DenialEvidence {
                world_path: Some(world_path.to_path_buf()),
                ..DenialEvidence::default()
            },
        )
    })?;
    Ok(parse_key_value_lines(&raw))
}

fn parse_key_value_lines(raw: &str) -> BTreeMap<String, String> {
    raw.lines()
        .filter_map(|line| line.split_once('='))
        .map(|(key, value)| (key.trim().to_owned(), value.trim().to_owned()))
        .filter(|(key, _value)| !key.is_empty())
        .collect()
}

fn world_value(
    profile: &Profile,
    foreground: &ForegroundContext,
    world_path: &Path,
    world: &BTreeMap<String, String>,
    key: &'static str,
) -> TargetPolicyResult<String> {
    world
        .get(key)
        .cloned()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            denial(
                profile,
                foreground,
                "world_metadata_key_missing",
                format!("world.mt is missing {key}"),
                DenialEvidence {
                    world_path: Some(world_path.to_path_buf()),
                    ..DenialEvidence::default()
                },
            )
        })
}

fn approved_worlds(metadata: &BTreeMap<String, String>) -> BTreeSet<String> {
    metadata_trimmed(metadata, KEY_APPROVED_WORLDS)
        .unwrap_or_default()
        .split(|ch: char| ch == ',' || ch == ';' || ch.is_whitespace())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn read_latest_log_session(
    profile: &Profile,
    foreground: &ForegroundContext,
    world_path: &Path,
    logfile_path: &Path,
) -> TargetPolicyResult<String> {
    let raw = fs::read_to_string(logfile_path).map_err(|error| {
        denial(
            profile,
            foreground,
            "log_unreadable",
            format!("could not read {}: {error}", logfile_path.display()),
            DenialEvidence {
                world_path: Some(world_path.to_path_buf()),
                logfile_path: Some(logfile_path.to_path_buf()),
                ..DenialEvidence::default()
            },
        )
    })?;
    Ok(latest_log_session(&raw))
}

fn latest_log_session(raw: &str) -> String {
    let lines = raw.lines().collect::<Vec<_>>();
    let start = lines
        .iter()
        .rposition(|line| line.trim() == "Separator")
        .map_or(0, |index| index + 1);
    lines[start..].join("\n")
}

fn log_session_mentions_world(session: &str, world_path: &Path) -> bool {
    comparable_log_text(session).contains(&comparable_path(world_path))
}

fn log_session_mentions_gameid(session: &str, gameid: &str) -> bool {
    session.contains(&format!("Server for gameid=\"{gameid}\""))
}

fn comparable_log_text(value: &str) -> String {
    value.replace('/', "\\").to_ascii_lowercase()
}

fn denial(
    profile: &Profile,
    foreground: &ForegroundContext,
    reason: &'static str,
    detail: String,
    evidence: DenialEvidence,
) -> Box<TargetPolicyDenial> {
    Box::new(TargetPolicyDenial {
        reason,
        detail,
        profile_id: profile.id.clone(),
        foreground_pid: foreground.pid,
        foreground_process_path: foreground.process_path.clone(),
        evidence,
    })
}

fn target_policy_denied_error(tool: &'static str, denial: Box<TargetPolicyDenial>) -> ErrorData {
    tracing::warn!(
        code = error_codes::SAFETY_PROFILE_ACTION_DENIED,
        tool,
        reason = denial.reason,
        detail = %denial.detail,
        profile_id = %denial.profile_id,
        foreground_pid = denial.foreground_pid,
        foreground_process_path = %denial.foreground_process_path,
        world_path = ?denial.evidence.world_path.as_ref().map(|path| path.display().to_string()),
        logfile_path = ?denial.evidence.logfile_path.as_ref().map(|path| path.display().to_string()),
        observed_world_name = ?denial.evidence.observed_world_name,
        observed_gameid = ?denial.evidence.observed_gameid,
        process_command_line = ?denial.evidence.process_command_line,
        "supported_use target policy denied action dispatch"
    );
    let denial = *denial;
    ErrorData::new(
        ErrorCode(-32099),
        format!(
            "supported_use policy denied {tool} for profile {}: {}",
            denial.profile_id, denial.detail
        ),
        Some(json!({
            "code": error_codes::SAFETY_PROFILE_ACTION_DENIED,
            "tool": tool,
            "reason": denial.reason,
            "detail": denial.detail,
            "profile_id": denial.profile_id,
            "foreground_pid": denial.foreground_pid,
            "foreground_process_path": denial.foreground_process_path,
            "world_path": denial.evidence.world_path.map(|path| path.display().to_string()),
            "logfile_path": denial.evidence.logfile_path.map(|path| path.display().to_string()),
            "observed_world_name": denial.evidence.observed_world_name,
            "observed_gameid": denial.evidence.observed_gameid,
            "process_command_line": denial.evidence.process_command_line,
        })),
    )
}

fn target_policy_internal_error(tool: &'static str, detail: &str) -> ErrorData {
    ErrorData::new(
        ErrorCode(-32099),
        format!("supported_use policy could not read target state for {tool}: {detail}"),
        Some(json!({
            "code": error_codes::TOOL_INTERNAL_ERROR,
            "tool": tool,
            "reason": "target_policy_internal_error",
            "detail": detail,
        })),
    )
}
