use std::{
    collections::BTreeSet,
    fmt,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use rmcp::{ErrorData, model::ErrorCode};
use serde_json::json;
use synapse_core::{Action, Backend, ComboInput, ComboStep, error_codes};

pub type RequiredPermissions = BTreeSet<Permission>;

#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum Permission {
    ReadEvents,
    WriteReflex,
    ReadReflex,
    ReadProfile,
    WriteProfileActive,
    WriteReplay,
    ReadAudio,
    ReadStorage,
    WriteStorage,
    InputKeyboard,
    InputMouse,
    InputPad,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PermissionGrants {
    granted: RequiredPermissions,
}

impl PermissionGrants {
    pub fn from_config(raw: Option<&str>, audio_enabled: bool) -> Result<Self> {
        let granted = match raw {
            Some(raw) => parse_grants(raw)?,
            None => default_grants(audio_enabled),
        };
        if granted.contains(&Permission::ReadAudio) && !audio_enabled {
            bail!("READ_AUDIO requires --enable-audio or SYNAPSE_ENABLE_AUDIO=true");
        }
        Ok(Self { granted })
    }

    #[must_use]
    pub fn first_missing(&self, required: &RequiredPermissions) -> Option<Permission> {
        required
            .iter()
            .find(|permission| !self.granted.contains(permission))
            .copied()
    }

    #[must_use]
    pub fn names(&self) -> Vec<&'static str> {
        self.granted
            .iter()
            .map(|permission| permission.as_str())
            .collect()
    }
}

impl Permission {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ReadEvents => "READ_EVENTS",
            Self::WriteReflex => "WRITE_REFLEX",
            Self::ReadReflex => "READ_REFLEX",
            Self::ReadProfile => "READ_PROFILE",
            Self::WriteProfileActive => "WRITE_PROFILE_ACTIVE",
            Self::WriteReplay => "WRITE_REPLAY",
            Self::ReadAudio => "READ_AUDIO",
            Self::ReadStorage => "READ_STORAGE",
            Self::WriteStorage => "WRITE_STORAGE",
            Self::InputKeyboard => "INPUT_KEYBOARD",
            Self::InputMouse => "INPUT_MOUSE",
            Self::InputPad => "INPUT_PAD",
        }
    }

    fn parse(raw: &str) -> Result<Self> {
        let normalized = raw.trim().replace(['-', ' '], "_").to_ascii_uppercase();
        match normalized.as_str() {
            "READ_EVENTS" => Ok(Self::ReadEvents),
            "WRITE_REFLEX" => Ok(Self::WriteReflex),
            "READ_REFLEX" => Ok(Self::ReadReflex),
            "READ_PROFILE" => Ok(Self::ReadProfile),
            "WRITE_PROFILE_ACTIVE" => Ok(Self::WriteProfileActive),
            "WRITE_REPLAY" => Ok(Self::WriteReplay),
            "READ_AUDIO" => Ok(Self::ReadAudio),
            "READ_STORAGE" => Ok(Self::ReadStorage),
            "WRITE_STORAGE" => Ok(Self::WriteStorage),
            "INPUT_KEYBOARD" | "KEYBOARD" => Ok(Self::InputKeyboard),
            "INPUT_MOUSE" | "MOUSE" => Ok(Self::InputMouse),
            "INPUT_PAD" | "PAD" => Ok(Self::InputPad),
            other => bail!("unknown M3 permission {other:?}"),
        }
    }
}

impl fmt::Display for Permission {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[must_use]
pub fn required<const N: usize>(permissions: [Permission; N]) -> RequiredPermissions {
    permissions.into_iter().collect()
}

pub fn authorization_error(tool: &str, missing: Permission) -> ErrorData {
    ErrorData::new(
        ErrorCode(-32099),
        format!("tool {tool} requires permission {missing}"),
        Some(json!({
            "code": error_codes::SAFETY_PERMISSION_DENIED,
            "tool": tool,
            "missing_permission": missing.as_str(),
        })),
    )
}

pub fn profile_scope_error(profile_id: &str) -> ErrorData {
    ErrorData::new(
        ErrorCode(-32099),
        format!(
            "profile {profile_id} has use_scope=\"unknown\"; start with --allow-unknown-profile to activate it"
        ),
        Some(json!({
            "code": error_codes::SAFETY_PROFILE_ACTION_DENIED,
            "profile_id": profile_id,
            "use_scope": "unknown",
        })),
    )
}

pub fn replay_path_error(path: &Path, root: &Path) -> ErrorData {
    ErrorData::new(
        ErrorCode(-32099),
        format!(
            "replay_record path {} must stay under {}",
            path.display(),
            root.display()
        ),
        Some(json!({
            "code": error_codes::SAFETY_PERMISSION_DENIED,
            "permission": Permission::WriteReplay.as_str(),
            "reason": "path_outside_allow_root",
            "path": path.display().to_string(),
            "allow_root": root.display().to_string(),
        })),
    )
}

pub fn add_action_permissions(action: &Action, required: &mut RequiredPermissions) {
    match action {
        Action::KeyPress { backend, .. }
        | Action::KeyDown { backend, .. }
        | Action::KeyUp { backend, .. }
        | Action::KeyChord { backend, .. }
        | Action::TypeText { backend, .. } => {
            let _ = backend;
            required.insert(Permission::InputKeyboard);
        }
        Action::MouseMove { backend, .. }
        | Action::MouseMoveRelative { backend, .. }
        | Action::MouseButton { backend, .. }
        | Action::MouseDrag { backend, .. }
        | Action::MouseStroke { backend, .. }
        | Action::MouseScroll { backend, .. }
        | Action::AimAt { backend, .. } => {
            let _ = backend;
            required.insert(Permission::InputMouse);
        }
        Action::PadButton { .. }
        | Action::PadStick { .. }
        | Action::PadTrigger { .. }
        | Action::PadReport { .. } => {
            required.insert(Permission::InputPad);
        }
        Action::Combo { steps, backend } => {
            let _ = backend;
            add_combo_step_permissions(steps, *backend, required);
        }
        Action::ReleaseAll => {
            required.insert(Permission::InputKeyboard);
            required.insert(Permission::InputMouse);
            required.insert(Permission::InputPad);
        }
    }
}

pub fn normalize_replay_path(
    root: &Path,
    path: Option<&str>,
) -> std::result::Result<PathBuf, ErrorData> {
    let Some(raw_path) = path.map(str::trim) else {
        return Ok(default_replay_path(root));
    };
    if raw_path.is_empty() {
        return Err(crate::m1::mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "replay_record path must not be empty when provided",
        ));
    }

    let requested = PathBuf::from(raw_path);
    let candidate = if requested.is_absolute() {
        requested
    } else {
        root.join(requested)
    };
    let root = lexical_normalize(root);
    let candidate = lexical_normalize(&candidate);
    if is_under_root(&candidate, &root) {
        Ok(candidate)
    } else {
        Err(replay_path_error(&candidate, &root))
    }
}

#[must_use]
pub fn replay_root() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map_or_else(std::env::temp_dir, PathBuf::from)
        .join("synapse")
        .join("replays")
}

fn parse_grants(raw: &str) -> Result<RequiredPermissions> {
    let trimmed = raw.trim();
    if matches!(
        trimmed.to_ascii_uppercase().as_str(),
        "" | "NONE" | "DENY_ALL"
    ) {
        return Ok(RequiredPermissions::new());
    }
    let mut granted = RequiredPermissions::new();
    for token in trimmed
        .split(|ch: char| ch == ',' || ch == ';' || ch.is_whitespace())
        .filter(|token| !token.trim().is_empty())
    {
        granted.insert(Permission::parse(token)?);
    }
    Ok(granted)
}

fn default_grants(audio_enabled: bool) -> RequiredPermissions {
    // Fail closed by default: a stock daemon may read state, but write and
    // synthetic-input capabilities require explicit operator opt-in.
    let mut granted = required([
        Permission::ReadEvents,
        Permission::ReadReflex,
        Permission::ReadProfile,
        Permission::ReadStorage,
    ]);
    if audio_enabled {
        granted.insert(Permission::ReadAudio);
    }
    granted
}

fn add_combo_step_permissions(
    steps: &[ComboStep],
    _backend: Backend,
    required: &mut RequiredPermissions,
) {
    for step in steps {
        match step.input {
            ComboInput::KeyDown { .. } | ComboInput::KeyUp { .. } | ComboInput::KeyPress { .. } => {
                required.insert(Permission::InputKeyboard);
            }
            ComboInput::MouseButton { .. } | ComboInput::MouseMoveRel { .. } => {
                required.insert(Permission::InputMouse);
            }
            ComboInput::PadButton { .. } | ComboInput::PadStick { .. } => {
                required.insert(Permission::InputPad);
            }
        }
    }
}

fn default_replay_path(root: &Path) -> PathBuf {
    root.join(format!("replay-{}.jsonl", synapse_core::new_session_id()))
}

fn lexical_normalize(path: &Path) -> PathBuf {
    let raw = path.to_string_lossy().replace('\\', "/");
    let (prefix, rest) = split_windows_drive_prefix(&raw);
    let rooted = rest.starts_with('/');
    let mut parts = Vec::new();

    for part in rest.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if parts.last().is_some_and(|last| *last != "..") {
                    parts.pop();
                } else if !rooted && prefix.is_empty() {
                    parts.push(part);
                }
            }
            _ => parts.push(part),
        }
    }

    let separator = if prefix.is_empty() { "/" } else { "\\" };
    let mut normalized = String::new();
    if !prefix.is_empty() {
        normalized.push_str(prefix);
        if rooted {
            normalized.push('\\');
        }
    } else if rooted {
        normalized.push('/');
    }
    normalized.push_str(&parts.join(separator));
    PathBuf::from(normalized)
}

fn split_windows_drive_prefix(raw: &str) -> (&str, &str) {
    if raw.len() >= 2 && raw.as_bytes()[1] == b':' && raw.as_bytes()[0].is_ascii_alphabetic() {
        raw.split_at(2)
    } else {
        ("", raw)
    }
}

fn is_under_root(path: &Path, root: &Path) -> bool {
    let path_key = comparable_path(path);
    let root_key = comparable_path(root);
    path_key == root_key
        || path_key
            .strip_prefix(&root_key)
            .is_some_and(|suffix| suffix.starts_with(['\\', '/']))
}

fn comparable_path(path: &Path) -> String {
    path.to_string_lossy()
        .replace('/', "\\")
        .to_ascii_lowercase()
}

pub fn configured_grants_from_parts(
    raw: Option<&str>,
    audio_enabled: bool,
) -> Result<PermissionGrants> {
    PermissionGrants::from_config(raw, audio_enabled).context("parse M3 permission grants")
}
