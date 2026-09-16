use std::{
    collections::{BTreeMap, HashSet},
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use regex::Regex;
use synapse_core::{
    Backend, HudRegion, OcrBackend, PerceptionMode, Profile, ProfileCaptureTarget, ProfileMatch,
    ProfileUseScope,
};
use tracing::instrument;

use crate::error::ProfileError;
use crate::toml_format::RawProfile;

const DEFAULT_CAPTURE_INTERVAL_MS: u32 = 50;
const DEFAULT_CONFIDENCE_THRESHOLD: f32 = 0.5;
const DEFAULT_MAX_DETECTIONS: u32 = 32;
const DEFAULT_SCREEN_WIDTH: i32 = 3840;
const DEFAULT_SCREEN_HEIGHT: i32 = 2160;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProfileDefaults {
    pub mouse_velocity_profile_default: String,
    pub keyboard_dynamics_default: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct LoadedProfile {
    pub profile: Profile,
    pub schema_version: u32,
    pub defaults: ProfileDefaults,
    pub source_path: PathBuf,
    pub modified: SystemTime,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct ScreenBounds {
    pub width: i32,
    pub height: i32,
}

impl Default for ScreenBounds {
    fn default() -> Self {
        Self {
            width: DEFAULT_SCREEN_WIDTH,
            height: DEFAULT_SCREEN_HEIGHT,
        }
    }
}

#[instrument(skip_all, fields(path = %path.as_ref().display()))]
/// Parses a TOML profile file using default 3840x2160 HUD bounds.
///
/// # Errors
///
/// Returns [`ProfileError`] when the file cannot be read or its TOML, schema,
/// keymap, match, backend, or HUD values are invalid.
pub fn parse_profile_file(path: impl AsRef<Path>) -> Result<LoadedProfile, ProfileError> {
    parse_profile_file_with_bounds(path, ScreenBounds::default())
}

#[instrument(skip_all, fields(path = %path.as_ref().display(), screen_width = bounds.width, screen_height = bounds.height))]
/// Parses a TOML profile file using caller-supplied HUD bounds.
///
/// # Errors
///
/// Returns [`ProfileError`] when disk IO fails or profile contents violate the
/// supported schema and validation rules.
pub fn parse_profile_file_with_bounds(
    path: impl AsRef<Path>,
    bounds: ScreenBounds,
) -> Result<LoadedProfile, ProfileError> {
    let path = path.as_ref();
    let bytes = fs::read(path).map_err(|source| ProfileError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let modified = fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .unwrap_or(UNIX_EPOCH);
    parse_profile_bytes(path, &bytes, modified, bounds)
}

#[instrument(skip_all, fields(path = %path.as_ref().display(), bytes = bytes.len()))]
/// Parses profile TOML bytes with known source metadata.
///
/// # Errors
///
/// Returns [`ProfileError`] when the bytes are malformed TOML, target a future
/// schema version, or fail profile validation.
pub fn parse_profile_bytes(
    path: impl AsRef<Path>,
    bytes: &[u8],
    modified: SystemTime,
    bounds: ScreenBounds,
) -> Result<LoadedProfile, ProfileError> {
    let path = path.as_ref().to_path_buf();
    let raw: RawProfile = toml::from_slice(bytes).map_err(|source| ProfileError::Parse {
        path: path.clone(),
        message: source.to_string(),
    })?;
    raw.into_loaded(path, modified, bounds)
}

/// Resolves the directory that holds the bundled application profiles.
///
/// Production installs ship the profiles **next to the installed binary** (the
/// installer copies them beside `synapse-mcp.exe`), so we resolve relative to the
/// running executable first. This keeps the path valid on any host regardless of
/// where the binary was built — unlike a compile-time `CARGO_MANIFEST_DIR` path,
/// which points at the *build machine's* source tree and does not exist on an
/// installed host (and is a transient drive letter when the binary is built over
/// a mapped network/9p mount). The source-tree path remains the fallback for dev
/// runs (`cargo run`, tests) launched from a checkout.
#[must_use]
#[instrument]
pub fn bundled_profiles_dir() -> PathBuf {
    if let Ok(exe) = std::env::current_exe()
        && let Some(exe_dir) = exe.parent()
    {
        let beside_exe = exe_dir.join("profiles");
        if beside_exe.is_dir() {
            return beside_exe;
        }
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("profiles")
}

pub(crate) fn natural_default(
    path: &Path,
    field: &str,
    value: &str,
) -> Result<String, ProfileError> {
    if value.eq_ignore_ascii_case("natural") {
        return Ok("natural".to_owned());
    }
    Err(ProfileError::Parse {
        path: path.to_path_buf(),
        message: format!("{field} must be \"natural\"; got {value:?}"),
    })
}

pub(crate) fn parse_use_scope(value: &str, path: &Path) -> Result<ProfileUseScope, ProfileError> {
    match value {
        "productivity" => Ok(ProfileUseScope::Productivity),
        "single_player" => Ok(ProfileUseScope::SinglePlayer),
        "operator_owned_test" => Ok(ProfileUseScope::OperatorOwnedTest),
        "sanctioned_research" => Ok(ProfileUseScope::SanctionedResearch),
        "unknown" => Ok(ProfileUseScope::Unknown),
        other => Err(ProfileError::Parse {
            path: path.to_path_buf(),
            message: format!("unknown use_scope {other:?}"),
        }),
    }
}

pub(crate) fn parse_mode(value: &str, path: &Path) -> Result<PerceptionMode, ProfileError> {
    match value {
        "a11y_only" => Ok(PerceptionMode::A11yOnly),
        "pixel_only" => Ok(PerceptionMode::PixelOnly),
        "hybrid" => Ok(PerceptionMode::Hybrid),
        "auto" => Ok(PerceptionMode::Auto),
        other => Err(ProfileError::Parse {
            path: path.to_path_buf(),
            message: format!("unknown mode {other:?}"),
        }),
    }
}

pub(crate) fn parse_capture_target(
    value: &str,
    path: &Path,
) -> Result<ProfileCaptureTarget, ProfileError> {
    match value {
        "foreground_window" => Ok(ProfileCaptureTarget::ForegroundWindow),
        "primary_monitor" => Ok(ProfileCaptureTarget::PrimaryMonitor),
        other => Err(ProfileError::Parse {
            path: path.to_path_buf(),
            message: format!("unknown capture target {other:?}"),
        }),
    }
}

pub(crate) fn parse_backend(value: &str, path: &Path) -> Result<Backend, ProfileError> {
    match value {
        "software" => Ok(Backend::Software),
        "vigem" => Ok(Backend::Vigem),
        "hardware" => Ok(Backend::Hardware),
        "auto" => Ok(Backend::Auto),
        other => Err(ProfileError::Parse {
            path: path.to_path_buf(),
            message: format!("unknown backend {other:?}"),
        }),
    }
}

pub(crate) fn parse_ocr_backend(value: &str, path: &Path) -> Result<OcrBackend, ProfileError> {
    match value {
        "winrt" => Ok(OcrBackend::Winrt),
        "crnn" => Ok(OcrBackend::Crnn),
        "auto" => Ok(OcrBackend::Auto),
        other => Err(ProfileError::Parse {
            path: path.to_path_buf(),
            message: format!("unknown OCR backend {other:?}"),
        }),
    }
}

pub(crate) fn validate_keymap(
    path: &Path,
    keymap: &BTreeMap<String, String>,
) -> Result<(), ProfileError> {
    for (alias, binding) in keymap {
        if alias.trim().is_empty() {
            return Err(ProfileError::KeymapInvalid {
                path: path.to_path_buf(),
                alias: alias.clone(),
                binding: binding.clone(),
                message: "alias must be non-empty".to_owned(),
            });
        }
        let mut seen = HashSet::new();
        for raw_key in binding.split('+') {
            let key = canonical_key_name(raw_key).ok_or_else(|| ProfileError::KeymapInvalid {
                path: path.to_path_buf(),
                alias: alias.clone(),
                binding: binding.clone(),
                message: format!("unsupported key {raw_key:?}"),
            })?;
            if !seen.insert(key.clone()) {
                return Err(ProfileError::KeymapInvalid {
                    path: path.to_path_buf(),
                    alias: alias.clone(),
                    binding: binding.clone(),
                    message: format!("duplicate key {key:?}"),
                });
            }
        }
    }
    Ok(())
}

fn canonical_key_name(raw_key: &str) -> Option<String> {
    let lowered = raw_key.trim().to_ascii_lowercase();
    let key = match lowered.as_str() {
        "" => return None,
        "control" => "ctrl",
        "escape" => "esc",
        "return" => "enter",
        "backtick" | "grave" | "graveaccent" | "keyboardgraveaccent" => "`",
        "leftshift" => "lshift",
        "rightshift" => "rshift",
        "leftmouse" | "left_mouse" | "lmb" | "mouse_left" => "lmb",
        "rightmouse" | "right_mouse" | "rmb" | "mouse_right" => "rmb",
        "middlemouse" | "middle_mouse" | "mmb" | "mouse_middle" => "mmb",
        "arrowup" => "up",
        "arrowdown" => "down",
        "arrowleft" => "left",
        "arrowright" => "right",
        "win" | "windows" | "meta" => "super",
        "pgup" => "pageup",
        "pgdn" => "pagedown",
        other => other,
    };

    if key.len() == 1 && key.as_bytes()[0].is_ascii_alphanumeric() {
        return Some(key.to_owned());
    }
    if key
        .strip_prefix('f')
        .and_then(|suffix| suffix.parse::<u8>().ok())
        .is_some_and(|number| (1..=24).contains(&number))
    {
        return Some(key.to_owned());
    }
    match key {
        "`" | "alt" | "backspace" | "ctrl" | "delete" | "down" | "end" | "enter" | "esc"
        | "home" | "insert" | "left" | "lmb" | "mmb" | "pagedown" | "pageup" | "right" | "rmb"
        | "lshift" | "rshift" | "shift" | "space" | "super" | "tab" | "up" | "x1" | "x2" => {
            Some(key.to_owned())
        }
        _ => None,
    }
}

pub(crate) fn validate_match(
    path: &Path,
    profile_match: &ProfileMatch,
) -> Result<(), ProfileError> {
    if profile_match.exe.is_none()
        && profile_match.title_regex.is_none()
        && profile_match.steam_appid.is_none()
        && profile_match.window_class.is_none()
    {
        return Err(ProfileError::Parse {
            path: path.to_path_buf(),
            message: "each profile match must define at least one matcher".to_owned(),
        });
    }
    if let Some(pattern) = &profile_match.title_regex {
        Regex::new(pattern).map_err(|source| ProfileError::Parse {
            path: path.to_path_buf(),
            message: format!("invalid title_regex {pattern:?}: {source}"),
        })?;
    }
    Ok(())
}

pub(crate) fn validate_hud_region(
    path: &Path,
    name: &str,
    region: &HudRegion,
    bounds: ScreenBounds,
) -> Result<(), ProfileError> {
    match *region {
        HudRegion::Absolute { x, y, w, h } => {
            if x < 0 || y < 0 || w <= 0 || h <= 0 || x + w > bounds.width || y + h > bounds.height {
                return Err(ProfileError::HudRegionInvalid {
                    path: path.to_path_buf(),
                    name: name.to_owned(),
                    message: format!(
                        "absolute region x={x} y={y} w={w} h={h} outside {}x{}",
                        bounds.width, bounds.height
                    ),
                });
            }
        }
        HudRegion::FractionOfWindow { x, y, w, h } => {
            if x < 0.0 || y < 0.0 || w <= 0.0 || h <= 0.0 || x + w > 1.0 || y + h > 1.0 {
                return Err(ProfileError::HudRegionInvalid {
                    path: path.to_path_buf(),
                    name: name.to_owned(),
                    message: format!("fractional region x={x} y={y} w={w} h={h} outside unit box"),
                });
            }
        }
        HudRegion::AnchoredToEdge { w, h, .. } => {
            if w <= 0 || h <= 0 || w > bounds.width || h > bounds.height {
                return Err(ProfileError::HudRegionInvalid {
                    path: path.to_path_buf(),
                    name: name.to_owned(),
                    message: format!(
                        "anchored region w={w} h={h} outside {}x{}",
                        bounds.width, bounds.height
                    ),
                });
            }
        }
    }
    Ok(())
}

pub(crate) fn default_mode() -> String {
    "a11y_only".to_owned()
}

pub(crate) fn default_capture_target() -> String {
    "foreground_window".to_owned()
}

pub(crate) const fn default_capture_interval() -> u32 {
    DEFAULT_CAPTURE_INTERVAL_MS
}

pub(crate) const fn default_cursor_visible() -> bool {
    true
}

pub(crate) const fn default_confidence_threshold() -> f32 {
    DEFAULT_CONFIDENCE_THRESHOLD
}

pub(crate) const fn default_max_detections() -> u32 {
    DEFAULT_MAX_DETECTIONS
}

pub(crate) fn default_ocr_backend() -> String {
    "auto".to_owned()
}

pub(crate) fn default_backend() -> String {
    "auto".to_owned()
}

pub(crate) fn default_hud_region_kind() -> String {
    "absolute".to_owned()
}
