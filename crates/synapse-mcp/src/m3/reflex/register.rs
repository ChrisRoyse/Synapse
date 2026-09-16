use std::{
    collections::HashSet,
    sync::{Arc, Mutex},
    time::Duration,
};

use rmcp::ErrorData;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use synapse_core::{
    Action, AimTarget, Backend, HumanizeParams, Key, KeyCode, MouseButton, PathSpec, ReflexAimAxis,
    ReflexButtonTarget, ReflexLifetime, ReflexStatus, StrokeMotionModel, StrokeTiming,
    VelocityProfile, error_codes, new_reflex_id,
};
use synapse_reflex::{
    AimTrackParams, AimTrackTarget, ComboParams, HoldButtonParams, HoldMoveParams,
    PathFollowParams, ReflexError, ReflexRuntime, ScheduledReflex,
};

use crate::m1::mcp_error;

use super::{
    super::permissions::{Permission, RequiredPermissions, add_action_permissions, required},
    common::{
        ReflexComboStepParam, ReflexThenParam, ReflexWhenParam, actions_from_then,
        button_down_action, combo_steps_from_params, default_backend, default_lifetime,
        default_reflex_priority, reflex_kind_schema, required_then,
    },
};

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReflexRegisterParams {
    #[schemars(schema_with = "reflex_kind_schema")]
    pub kind: String,
    #[serde(default)]
    pub when: Option<ReflexWhenParam>,
    #[serde(default)]
    pub then: Option<ReflexThenParam>,
    #[serde(default)]
    pub debounce_ms: u32,
    #[serde(default)]
    pub target: Option<AimTarget>,
    #[serde(default)]
    pub axis: Option<ReflexAimAxis>,
    #[serde(default)]
    pub gain: Option<f32>,
    #[serde(default)]
    pub deadzone_px: Option<f32>,
    #[serde(default)]
    pub max_speed_px_per_tick: Option<f32>,
    #[serde(default)]
    pub ema_alpha: Option<f32>,
    #[serde(default)]
    pub key: Option<String>,
    #[serde(default)]
    pub keys: Option<Vec<String>>,
    #[serde(default)]
    pub re_assert: bool,
    #[serde(default)]
    pub button: Option<ReflexButtonParam>,
    #[serde(default)]
    pub steps: Option<Vec<ReflexComboStepParam>>,
    #[serde(default)]
    pub path: Option<PathSpec>,
    #[serde(default = "default_path_follow_velocity_profile")]
    #[schemars(default = "default_path_follow_velocity_profile")]
    pub velocity_profile: VelocityProfile,
    #[serde(default)]
    pub duration_or_speed: Option<StrokeTiming>,
    #[serde(default)]
    pub humanize: Option<HumanizeParams>,
    #[serde(default = "default_reflex_priority")]
    #[schemars(default = "default_reflex_priority", range(min = 0, max = 1000))]
    pub priority: u32,
    #[serde(default = "default_lifetime")]
    #[schemars(default = "default_lifetime")]
    pub lifetime: ReflexLifetime,
    #[serde(default = "default_backend")]
    #[schemars(default = "default_backend")]
    pub backend: Backend,
    #[serde(default)]
    pub exclusive: bool,
}

impl ReflexRegisterParams {
    pub(crate) fn file_jsonl_tail_watcher_request(
        &self,
        reflex_id: String,
    ) -> Option<super::FileJsonlTailWatcherRequest> {
        if self.kind != "on_event" {
            return None;
        }
        let when = self.when.as_ref()?.file_jsonl_tail()?.clone();
        Some(super::FileJsonlTailWatcherRequest {
            reflex_id,
            when,
            lifetime: self.lifetime.clone(),
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReflexRegisterResponse {
    pub reflex_id: String,
    pub state: ReflexStatus,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum ReflexButtonParam {
    Target(ReflexButtonTarget),
    Mouse(MouseButton),
}

impl ReflexButtonParam {
    fn into_target(self) -> ReflexButtonTarget {
        match self {
            Self::Target(target) => target,
            Self::Mouse(button) => ReflexButtonTarget::Mouse { button },
        }
    }

    fn into_mouse_button(self, kind: &'static str) -> Result<MouseButton, ReflexError> {
        match self {
            Self::Mouse(button) | Self::Target(ReflexButtonTarget::Mouse { button }) => Ok(button),
            Self::Target(ReflexButtonTarget::Pad { .. }) => Err(ReflexError::ParamsInvalid {
                detail: format!("{kind} reflex requires a mouse button when button is provided"),
            }),
        }
    }
}

pub fn required_permissions_register(
    params: &ReflexRegisterParams,
) -> Result<RequiredPermissions, ErrorData> {
    let mut permissions = required([Permission::WriteReflex]);
    let actions = actions_for_permissions(params)
        .map_err(|error| mcp_error(error.code(), error.to_string()))?;
    for action in &actions {
        add_action_permissions(action, &mut permissions);
    }
    Ok(permissions)
}

#[must_use]
pub fn requires_a11y_event_bridge(params: &ReflexRegisterParams) -> bool {
    params.kind == "on_event"
        && params
            .when
            .as_ref()
            .is_some_and(ReflexWhenParam::requires_a11y_event_bridge)
}

pub fn register_reflex(
    runtime: &Arc<Mutex<ReflexRuntime>>,
    params: ReflexRegisterParams,
) -> Result<ReflexRegisterResponse, ErrorData> {
    let reflex = scheduled_reflex_from_params(params)
        .map_err(|error| mcp_error(error.code(), error.to_string()))?;
    let mut runtime = runtime.lock().map_err(|_err| {
        mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            "reflex runtime lock poisoned",
        )
    })?;
    let state = runtime
        .register(&reflex)
        .map_err(|error| mcp_error(error.code(), error.to_string()))?;
    drop(runtime);
    Ok(ReflexRegisterResponse {
        reflex_id: state.id.clone(),
        state,
    })
}

pub(super) fn scheduled_reflex_from_params(
    params: ReflexRegisterParams,
) -> Result<ScheduledReflex, ReflexError> {
    let reflex_id = new_reflex_id();
    match params.kind.as_str() {
        "on_event" => {
            let when = params.when.ok_or_else(|| ReflexError::ParamsInvalid {
                detail: "on_event reflex requires when filter".to_owned(),
            })?;
            let when = when.into_event_filter()?;
            let actions =
                actions_from_then(required_then(params.then, "on_event")?, params.backend)?;
            let debounce = Duration::from_millis(u64::from(params.debounce_ms));
            let reflex = if debounce.is_zero() {
                ScheduledReflex::on_event(reflex_id, when, actions)
            } else {
                ScheduledReflex::on_event_with_debounce(reflex_id, when, actions, debounce)
            };
            Ok(reflex
                .with_priority(params.priority)
                .with_lifetime(params.lifetime)
                .with_exclusive(params.exclusive))
        }
        "aim_track" => {
            let mut aim_params = aim_track_params(&params)?;
            aim_params.backend = params.backend;
            Ok(ScheduledReflex::aim_track(reflex_id, aim_params)
                .with_priority(params.priority)
                .with_lifetime(params.lifetime)
                .with_exclusive(params.exclusive))
        }
        "hold_move" => {
            let hold_params = HoldMoveParams {
                keys: hold_move_keys(&params)?,
                backend: params.backend,
                re_assert: params.re_assert,
            };
            Ok(ScheduledReflex::hold_move(reflex_id, hold_params)
                .with_priority(params.priority)
                .with_lifetime(params.lifetime)
                .with_exclusive(params.exclusive))
        }
        "hold_button" => {
            let button = params
                .button
                .map(ReflexButtonParam::into_target)
                .ok_or_else(|| ReflexError::ParamsInvalid {
                    detail: "hold_button reflex requires button".to_owned(),
                })?;
            let hold_params = HoldButtonParams {
                button,
                backend: params.backend,
            };
            Ok(ScheduledReflex::hold_button(reflex_id, hold_params)
                .with_priority(params.priority)
                .with_lifetime(params.lifetime)
                .with_exclusive(params.exclusive))
        }
        "combo" => {
            let steps = combo_steps_from_params(params.steps, params.then)?;
            Ok(
                ScheduledReflex::combo(reflex_id, ComboParams::new(steps, params.backend))
                    .with_priority(params.priority)
                    .with_lifetime(ReflexLifetime::OneShot)
                    .with_exclusive(params.exclusive),
            )
        }
        "path_follow" => {
            let path_follow = path_follow_params(&params)?;
            Ok(ScheduledReflex::path_follow(reflex_id, path_follow)
                .with_priority(params.priority)
                .with_lifetime(ReflexLifetime::OneShot)
                .with_exclusive(params.exclusive))
        }
        other => Err(ReflexError::KindInvalid {
            detail: format!("unknown reflex kind: {other}"),
        }),
    }
}

fn actions_for_permissions(params: &ReflexRegisterParams) -> Result<Vec<Action>, ReflexError> {
    match params.kind.as_str() {
        "on_event" => actions_from_then(
            required_then(params.then.clone(), "on_event")?,
            params.backend,
        ),
        "aim_track" => Ok(vec![Action::MouseMoveRelative {
            dx: 0.0,
            dy: 0.0,
            backend: params.backend,
        }]),
        "hold_move" => hold_move_keys(params).map(|keys| {
            keys.into_iter()
                .map(|key| Action::KeyDown {
                    key,
                    backend: params.backend,
                })
                .collect()
        }),
        "hold_button" => {
            let button = params
                .button
                .clone()
                .map(ReflexButtonParam::into_target)
                .ok_or_else(|| ReflexError::ParamsInvalid {
                    detail: "hold_button reflex requires button".to_owned(),
                })?;
            Ok(vec![button_down_action(&button, params.backend)])
        }
        "combo" => Ok(vec![Action::Combo {
            steps: combo_steps_from_params(params.steps.clone(), params.then.clone())?,
            backend: params.backend,
        }]),
        "path_follow" => {
            let path_follow = path_follow_params(params)?;
            Ok(vec![Action::MouseStroke {
                path: path_follow.path,
                button: path_follow.button,
                profile: path_follow.profile,
                timing: path_follow.timing,
                motion_model: StrokeMotionModel::Path,
                humanize: path_follow.humanize,
                backend: path_follow.backend,
            }])
        }
        _other => Ok(Vec::new()),
    }
}

fn aim_track_params(params: &ReflexRegisterParams) -> Result<AimTrackParams, ReflexError> {
    let target = params
        .target
        .clone()
        .ok_or_else(|| ReflexError::ParamsInvalid {
            detail: "aim_track reflex requires target".to_owned(),
        })?;
    let mut aim_params = AimTrackParams::new(AimTrackTarget::from(target));
    if let Some(axis) = params.axis {
        aim_params.axis = axis;
    }
    if let Some(gain) = params.gain {
        aim_params.gain = gain;
    }
    if let Some(deadzone_px) = params.deadzone_px {
        aim_params.deadzone_px = deadzone_px;
    }
    if let Some(max_speed_px_per_tick) = params.max_speed_px_per_tick {
        aim_params.max_speed_px_per_tick = max_speed_px_per_tick;
    }
    if let Some(ema_alpha) = params.ema_alpha {
        aim_params.ema_alpha = ema_alpha;
    }
    Ok(aim_params)
}

fn path_follow_params(params: &ReflexRegisterParams) -> Result<PathFollowParams, ReflexError> {
    let path = params
        .path
        .clone()
        .ok_or_else(|| ReflexError::ParamsInvalid {
            detail: "path_follow reflex requires path".to_owned(),
        })?;
    let timing = params
        .duration_or_speed
        .clone()
        .ok_or_else(|| ReflexError::ParamsInvalid {
            detail: "path_follow reflex requires duration_or_speed".to_owned(),
        })?;
    let button = params
        .button
        .clone()
        .map(|button| button.into_mouse_button("path_follow"))
        .transpose()?;
    Ok(PathFollowParams::new(
        path,
        button,
        params.velocity_profile,
        timing,
        params.humanize,
        params.backend,
    ))
}

fn hold_move_keys(params: &ReflexRegisterParams) -> Result<Vec<Key>, ReflexError> {
    let mut raw = Vec::new();
    if let Some(key) = &params.key {
        raw.push(key.clone());
    }
    if let Some(keys) = &params.keys {
        raw.extend(keys.clone());
    }
    if raw.is_empty() {
        return Err(ReflexError::ParamsInvalid {
            detail: "hold_move reflex requires key or keys".to_owned(),
        });
    }
    let mut seen = HashSet::new();
    raw.into_iter()
        .map(|raw_key| {
            let name = canonical_key_name(&raw_key)?;
            if !seen.insert(name.clone()) {
                return Err(ReflexError::ParamsInvalid {
                    detail: format!("hold_move duplicate key '{name}'"),
                });
            }
            Ok(named_key(&name))
        })
        .collect()
}

fn canonical_key_name(raw_key: &str) -> Result<String, ReflexError> {
    let lowered = raw_key.trim().to_ascii_lowercase();
    let key = match lowered.as_str() {
        "" => {
            return Err(ReflexError::ParamsInvalid {
                detail: "key names must be non-empty".to_owned(),
            });
        }
        "control" => "ctrl",
        "escape" => "esc",
        "return" => "enter",
        "arrowup" => "up",
        "arrowdown" => "down",
        "arrowleft" => "left",
        "arrowright" => "right",
        "win" | "windows" | "meta" => "super",
        "pgup" => "pageup",
        "pgdn" => "pagedown",
        other => other,
    };

    if is_allowed_key_name(key) {
        Ok(key.to_owned())
    } else {
        Err(ReflexError::ParamsInvalid {
            detail: format!("unsupported key '{raw_key}'"),
        })
    }
}

fn is_allowed_key_name(key: &str) -> bool {
    if key.len() == 1 && key.as_bytes()[0].is_ascii_alphanumeric() {
        return true;
    }
    if let Some(number) = key
        .strip_prefix('f')
        .and_then(|suffix| suffix.parse::<u8>().ok())
    {
        return (1..=24).contains(&number);
    }
    matches!(
        key,
        "alt"
            | "backspace"
            | "ctrl"
            | "delete"
            | "down"
            | "end"
            | "enter"
            | "esc"
            | "home"
            | "insert"
            | "left"
            | "pagedown"
            | "pageup"
            | "right"
            | "shift"
            | "space"
            | "super"
            | "tab"
            | "up"
    )
}

fn named_key(value: &str) -> Key {
    Key {
        code: KeyCode::Named {
            value: value.to_owned(),
        },
        use_scancode: false,
    }
}

const fn default_path_follow_velocity_profile() -> VelocityProfile {
    VelocityProfile::Constant
}
