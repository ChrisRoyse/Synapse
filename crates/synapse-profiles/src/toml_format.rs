use std::{collections::BTreeMap, path::Path, path::PathBuf, time::SystemTime};

use serde::Deserialize;
use synapse_core::{
    EventExtension, HudExtractor, HudFieldSpec, HudParser, HudRegion, PROFILE_SCHEMA_VERSION,
    Profile, ProfileBackends, ProfileCapture, ProfileDetection, ProfileId, ProfileMatch,
    ProfileOcr, default_hud_confidence_threshold,
};

use crate::{
    error::ProfileError,
    parser::{
        LoadedProfile, ProfileDefaults, ScreenBounds, default_backend, default_capture_interval,
        default_capture_target, default_confidence_threshold, default_cursor_visible,
        default_hud_region_kind, default_max_detections, default_mode, default_ocr_backend,
        natural_default, parse_backend, parse_capture_target, parse_mode, parse_ocr_backend,
        parse_use_scope, validate_hud_region, validate_keymap, validate_match,
    },
};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawProfile {
    id: ProfileId,
    label: String,
    schema_version: u32,
    use_scope: String,
    #[serde(default = "default_mode")]
    mode: String,
    #[serde(default)]
    mouse_velocity_profile_default: Option<String>,
    #[serde(default)]
    mouse_curve_default: Option<String>,
    keyboard_dynamics_default: String,
    #[serde(default)]
    matches: Vec<RawProfileMatch>,
    #[serde(default)]
    capture: RawCapture,
    #[serde(default)]
    detection: RawDetection,
    #[serde(default)]
    ocr: RawOcr,
    #[serde(default)]
    hud: Vec<RawHudField>,
    #[serde(default)]
    keymap: BTreeMap<String, String>,
    #[serde(default)]
    backends: RawBackends,
    #[serde(default)]
    metadata: BTreeMap<String, String>,
    #[serde(default)]
    event_extensions: Vec<EventExtension>,
}

impl RawProfile {
    pub fn into_loaded(
        self,
        path: PathBuf,
        modified: SystemTime,
        bounds: ScreenBounds,
    ) -> Result<LoadedProfile, ProfileError> {
        if self.schema_version != PROFILE_SCHEMA_VERSION {
            return Err(ProfileError::VersionIncompatible {
                path,
                schema_version: self.schema_version,
                supported_version: PROFILE_SCHEMA_VERSION,
            });
        }
        if self.matches.is_empty() {
            return Err(ProfileError::Parse {
                path,
                message: "profile must contain at least one [[matches]] entry".to_owned(),
            });
        }
        let matches = self
            .matches
            .into_iter()
            .map(RawProfileMatch::into_match)
            .collect::<Vec<_>>();
        for profile_match in &matches {
            validate_match(&path, profile_match)?;
        }
        validate_keymap(&path, &self.keymap)?;
        validate_event_extensions(&path, &self.event_extensions)?;

        let hud = self
            .hud
            .into_iter()
            .map(|raw| raw.into_spec(&path, bounds))
            .collect::<Result<Vec<_>, _>>()?;

        let profile = Profile {
            id: self.id,
            label: self.label,
            version: self.schema_version.to_string(),
            use_scope: parse_use_scope(&self.use_scope, &path)?,
            matches,
            mode: parse_mode(&self.mode, &path)?,
            capture: self.capture.into_capture(&path)?,
            detection: self.detection.into_detection(),
            ocr: self.ocr.into_ocr(&path)?,
            hud,
            keymap: self.keymap,
            backends: self.backends.into_backends(&path)?,
            metadata: self.metadata,
            event_extensions: self.event_extensions,
        };
        let mouse_velocity_profile_default = match (
            self.mouse_velocity_profile_default,
            self.mouse_curve_default,
        ) {
            (Some(_), Some(_)) => {
                return Err(ProfileError::Parse {
                    path,
                    message: "profile accepts mouse_velocity_profile_default or deprecated mouse_curve_default, not both".to_owned(),
                });
            }
            (Some(value), None) => {
                natural_default(&path, "mouse_velocity_profile_default", &value)?
            }
            (None, Some(value)) => natural_default(&path, "mouse_curve_default", &value)?,
            (None, None) => {
                return Err(ProfileError::Parse {
                    path,
                    message: "profile is missing mouse_velocity_profile_default".to_owned(),
                });
            }
        };

        Ok(LoadedProfile {
            profile,
            schema_version: self.schema_version,
            defaults: ProfileDefaults {
                mouse_velocity_profile_default,
                keyboard_dynamics_default: natural_default(
                    &path,
                    "keyboard_dynamics_default",
                    &self.keyboard_dynamics_default,
                )?,
            },
            source_path: path,
            modified,
        })
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawProfileMatch {
    #[serde(default)]
    exe: Option<String>,
    #[serde(default)]
    title_regex: Option<String>,
    #[serde(default)]
    steam_appid: Option<u32>,
    #[serde(default)]
    window_class: Option<String>,
    #[serde(default)]
    process_args: Vec<String>,
}

impl RawProfileMatch {
    fn into_match(self) -> ProfileMatch {
        ProfileMatch {
            exe: self.exe,
            title_regex: self.title_regex,
            steam_appid: self.steam_appid,
            window_class: self.window_class,
            process_args: self.process_args,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCapture {
    #[serde(default = "default_capture_target")]
    target: String,
    #[serde(default = "default_capture_interval")]
    min_update_interval_ms: u32,
    #[serde(default = "default_cursor_visible")]
    cursor_visible: bool,
}

impl Default for RawCapture {
    fn default() -> Self {
        Self {
            target: default_capture_target(),
            min_update_interval_ms: default_capture_interval(),
            cursor_visible: default_cursor_visible(),
        }
    }
}

impl RawCapture {
    fn into_capture(self, path: &Path) -> Result<ProfileCapture, ProfileError> {
        Ok(ProfileCapture {
            target: parse_capture_target(&self.target, path)?,
            min_update_interval_ms: self.min_update_interval_ms,
            cursor_visible: self.cursor_visible,
        })
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDetection {
    #[serde(default)]
    model_id: Option<String>,
    #[serde(default)]
    classes_of_interest: Vec<String>,
    #[serde(default = "default_confidence_threshold")]
    confidence_threshold: f32,
    #[serde(default = "default_max_detections")]
    max_detections: u32,
}

impl Default for RawDetection {
    fn default() -> Self {
        Self {
            model_id: None,
            classes_of_interest: Vec::new(),
            confidence_threshold: default_confidence_threshold(),
            max_detections: default_max_detections(),
        }
    }
}

impl RawDetection {
    fn into_detection(self) -> ProfileDetection {
        ProfileDetection {
            model_id: self.model_id,
            classes_of_interest: self.classes_of_interest,
            confidence_threshold: self.confidence_threshold,
            max_detections: self.max_detections,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawOcr {
    #[serde(default = "default_ocr_backend")]
    default_backend: String,
}

impl Default for RawOcr {
    fn default() -> Self {
        Self {
            default_backend: default_ocr_backend(),
        }
    }
}

impl RawOcr {
    fn into_ocr(self, path: &Path) -> Result<ProfileOcr, ProfileError> {
        Ok(ProfileOcr {
            default_backend: parse_ocr_backend(&self.default_backend, path)?,
            regions: Vec::new(),
            parser_config: BTreeMap::new(),
        })
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawHudField {
    name: String,
    #[serde(default)]
    region: Option<HudRegion>,
    #[serde(default)]
    extractor: Option<HudExtractor>,
    #[serde(default)]
    parser: Option<HudParser>,
    #[serde(default = "default_hud_confidence_threshold")]
    confidence_threshold: f32,
    #[serde(default)]
    region_kind: Option<String>,
    #[serde(default)]
    x: Option<i32>,
    #[serde(default)]
    y: Option<i32>,
    #[serde(default)]
    w: Option<i32>,
    #[serde(default)]
    h: Option<i32>,
}

impl RawHudField {
    fn into_spec(self, path: &Path, bounds: ScreenBounds) -> Result<HudFieldSpec, ProfileError> {
        let region = match self.region {
            Some(region) => region,
            None => self.flat_region(path)?,
        };
        validate_hud_region(path, &self.name, &region, bounds)?;
        validate_hud_confidence_threshold(path, &self.name, self.confidence_threshold)?;
        let parser = self.parser.unwrap_or(HudParser::Number);
        validate_hud_parser(path, &self.name, &parser)?;
        Ok(HudFieldSpec {
            name: self.name,
            region,
            extractor: self.extractor.unwrap_or(HudExtractor::WinrtOcr),
            parser,
            confidence_threshold: self.confidence_threshold,
        })
    }

    fn flat_region(&self, path: &Path) -> Result<HudRegion, ProfileError> {
        let region_kind = self
            .region_kind
            .clone()
            .unwrap_or_else(default_hud_region_kind);
        match region_kind.as_str() {
            "absolute" => Ok(HudRegion::Absolute {
                x: required_hud_coord(path, &self.name, "x", self.x)?,
                y: required_hud_coord(path, &self.name, "y", self.y)?,
                w: required_hud_coord(path, &self.name, "w", self.w)?,
                h: required_hud_coord(path, &self.name, "h", self.h)?,
            }),
            other => Err(ProfileError::Parse {
                path: path.to_path_buf(),
                message: format!("unknown HUD region_kind {other:?}"),
            }),
        }
    }
}

fn validate_hud_confidence_threshold(
    path: &Path,
    name: &str,
    confidence_threshold: f32,
) -> Result<(), ProfileError> {
    if !confidence_threshold.is_finite() || !(0.0..=1.0).contains(&confidence_threshold) {
        return Err(ProfileError::Parse {
            path: path.to_path_buf(),
            message: format!(
                "HUD field {name:?} confidence_threshold must be finite and in 0..=1, got {confidence_threshold}"
            ),
        });
    }
    Ok(())
}

fn validate_hud_parser(path: &Path, name: &str, parser: &HudParser) -> Result<(), ProfileError> {
    if let HudParser::BoundedInteger {
        min,
        max,
        default_on_no_text,
    } = parser
    {
        if min > max {
            return Err(ProfileError::Parse {
                path: path.to_path_buf(),
                message: format!("HUD field {name:?} bounded_integer min {min} exceeds max {max}"),
            });
        }
        if let Some(default) = default_on_no_text
            && !(*min..=*max).contains(default)
        {
            return Err(ProfileError::Parse {
                path: path.to_path_buf(),
                message: format!(
                    "HUD field {name:?} bounded_integer default_on_no_text {default} outside {min}..={max}"
                ),
            });
        }
    }
    Ok(())
}

fn required_hud_coord(
    path: &Path,
    name: &str,
    field: &'static str,
    value: Option<i32>,
) -> Result<i32, ProfileError> {
    value.ok_or_else(|| ProfileError::Parse {
        path: path.to_path_buf(),
        message: format!("HUD field {name:?} is missing {field}"),
    })
}

fn validate_event_extensions(
    path: &Path,
    extensions: &[EventExtension],
) -> Result<(), ProfileError> {
    for extension in extensions {
        if extension.name.trim().is_empty() {
            return Err(ProfileError::Parse {
                path: path.to_path_buf(),
                message: "event extension name must not be empty".to_owned(),
            });
        }
        if extension.emits_kind.trim().is_empty() {
            return Err(ProfileError::Parse {
                path: path.to_path_buf(),
                message: format!(
                    "event extension {:?} emits_kind must not be empty",
                    extension.name
                ),
            });
        }
        extension
            .from_filter
            .validate()
            .map_err(|error| ProfileError::Parse {
                path: path.to_path_buf(),
                message: format!(
                    "event extension {:?} filter invalid: {error}",
                    extension.name
                ),
            })?;
        if extension.from_filter.is_trivially_always_true() {
            return Err(ProfileError::Parse {
                path: path.to_path_buf(),
                message: format!(
                    "event extension {:?} from_filter must not be trivially always true",
                    extension.name
                ),
            });
        }
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawBackends {
    #[serde(default = "default_backend", alias = "default_backend")]
    default: String,
    #[serde(default = "default_backend")]
    keyboard_default: String,
    #[serde(default = "default_backend")]
    mouse_default: String,
    #[serde(default = "default_backend")]
    pad_default: String,
}

impl Default for RawBackends {
    fn default() -> Self {
        Self {
            default: default_backend(),
            keyboard_default: default_backend(),
            mouse_default: default_backend(),
            pad_default: default_backend(),
        }
    }
}

impl RawBackends {
    fn into_backends(self, path: &Path) -> Result<ProfileBackends, ProfileError> {
        Ok(ProfileBackends {
            default: parse_backend(&self.default, path)?,
            keyboard_default: parse_backend(&self.keyboard_default, path)?,
            mouse_default: parse_backend(&self.mouse_default, path)?,
            pad_default: parse_backend(&self.pad_default, path)?,
        })
    }
}
