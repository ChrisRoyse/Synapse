use std::{collections::HashMap, sync::Arc, time::Duration, time::Instant};

use rmcp::ErrorData;
use rmcp::schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use synapse_action::{
    ActionBackend, ActionError, ActionHandle, EmitState, RecordedInput, RecordingBackend,
};
use synapse_core::{Action, GamepadController, GamepadReport, PadButton, PadId, error_codes};

use crate::m1::mcp_error;
use crate::m2::postcondition::{
    ActPostcondition, default_verify_timeout_ms, postcondition_not_requested,
};

const MAX_HOLD_MS: u32 = 30_000;

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ActPadParams {
    #[serde(default)]
    #[schemars(default)]
    pub pad_id: PadId,
    #[serde(default = "default_pad_controller")]
    #[schemars(default = "default_pad_controller")]
    pub controller: ActPadController,
    pub report: ActPadReport,
    #[serde(default = "default_pad_backend")]
    #[schemars(default = "default_pad_backend")]
    pub backend: PadBackend,
    pub hold_ms: Option<u32>,
    #[serde(default)]
    #[schemars(default)]
    pub verify_delta: bool,
    #[serde(default = "default_verify_timeout_ms")]
    #[schemars(default = "default_verify_timeout_ms", range(min = 50, max = 5000))]
    pub verify_timeout_ms: u32,
}

#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ActPadReport {
    #[serde(default)]
    pub buttons: Vec<ActPadButton>,
    #[serde(default = "neutral_axis")]
    #[schemars(schema_with = "normalized_axis_pair_schema", default = "neutral_axis")]
    pub thumb_l: (f32, f32),
    #[serde(default = "neutral_axis")]
    #[schemars(schema_with = "normalized_axis_pair_schema", default = "neutral_axis")]
    pub thumb_r: (f32, f32),
    #[serde(default)]
    #[schemars(range(min = 0.0, max = 1.0))]
    pub lt: f32,
    #[serde(default)]
    #[schemars(range(min = 0.0, max = 1.0))]
    pub rt: f32,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ActPadButton {
    A,
    B,
    X,
    Y,
    Lb,
    Rb,
    Ls,
    Rs,
    Back,
    Start,
    Up,
    Down,
    Left,
    Right,
    Guide,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ActPadController {
    X360,
    Ds4,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum PadBackend {
    Vigem,
    Hardware,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ActPadResponse {
    pub ok: bool,
    pub pad_id: PadId,
    pub controller: ActPadController,
    pub buttons: Vec<ActPadButton>,
    pub backend_used: String,
    pub backend_tier_used: String,
    pub required_foreground: bool,
    pub hold_ms: Option<u32>,
    pub returned_to_neutral: bool,
    pub elapsed_ms: u32,
    pub postcondition: ActPostcondition,
}

#[cfg(test)]
pub async fn act_pad_with_handle(
    handle: ActionHandle,
    recording: Option<Arc<RecordingBackend>>,
    params: ActPadParams,
) -> Result<ActPadResponse, ErrorData> {
    let boundary = super::OperatorPanicActionBoundary::arm("act_pad", "direct_call_entry")?;
    act_pad_with_handle_and_boundary(handle, recording, params, boundary).await
}

pub(crate) async fn act_pad_with_handle_and_boundary(
    handle: ActionHandle,
    recording: Option<Arc<RecordingBackend>>,
    params: ActPadParams,
    boundary: super::OperatorPanicActionBoundary,
) -> Result<ActPadResponse, ErrorData> {
    validate_params(&params)?;
    let started = Instant::now();
    let report = params.report.to_gamepad_report(params.controller)?;
    let neutral = neutral_report(params.controller);
    let report_action = Action::PadReport {
        pad: params.pad_id,
        report,
    };
    let neutral_action = Action::PadReport {
        pad: params.pad_id,
        report: neutral,
    };

    if let Some(recording) = recording {
        execute_recording(
            &recording,
            &report_action,
            params.hold_ms,
            &neutral_action,
            boundary,
        )?;
    } else {
        boundary.ensure("immediately_before_pad_report_dispatch")?;
        handle
            .execute(report_action)
            .await
            .map_err(|error| action_error_to_mcp(&error))?;
        if let Some(hold_ms) = params.hold_ms {
            tokio::time::sleep(Duration::from_millis(u64::from(hold_ms))).await;
            let boundary_error = boundary
                .ensure("after_pad_hold_before_neutral_cleanup")
                .err();
            let neutral_result = handle
                .execute(neutral_action)
                .await
                .map_err(|error| action_error_to_mcp(&error));
            if let Some(error) = boundary_error {
                if let Err(neutral_error) = neutral_result {
                    tracing::error!(
                        code = error_codes::SAFETY_OPERATOR_HOTKEY_FIRED,
                        detail_code = "PAD_NEUTRAL_AFTER_OPERATOR_PANIC_FAILED",
                        detail = %neutral_error,
                        "operator panic superseded a held pad report and best-effort neutral cleanup failed"
                    );
                }
                return Err(error);
            }
            neutral_result?;
        }
    }

    Ok(ActPadResponse {
        ok: true,
        pad_id: params.pad_id,
        controller: params.controller,
        buttons: params.report.buttons,
        backend_used: "vigem".to_owned(),
        backend_tier_used: "vigem".to_owned(),
        required_foreground: false,
        hold_ms: params.hold_ms,
        returned_to_neutral: params.hold_ms.is_some(),
        elapsed_ms: u32::try_from(started.elapsed().as_millis()).unwrap_or(u32::MAX),
        postcondition: postcondition_not_requested("act_pad", "action_emitter.pad_state"),
    })
}

impl ActPadReport {
    fn to_gamepad_report(&self, controller: ActPadController) -> Result<GamepadReport, ErrorData> {
        validate_axis_pair("thumb_l", self.thumb_l)?;
        validate_axis_pair("thumb_r", self.thumb_r)?;
        validate_trigger("lt", self.lt)?;
        validate_trigger("rt", self.rt)?;
        Ok(GamepadReport {
            controller: controller.to_gamepad_controller(),
            buttons: self
                .buttons
                .iter()
                .copied()
                .map(ActPadButton::to_pad_button)
                .collect(),
            thumb_l: self.thumb_l,
            thumb_r: self.thumb_r,
            lt: self.lt,
            rt: self.rt,
        })
    }
}

impl ActPadController {
    const fn to_gamepad_controller(self) -> GamepadController {
        match self {
            Self::X360 => GamepadController::X360,
            Self::Ds4 => GamepadController::Ds4,
        }
    }
}

impl ActPadButton {
    const fn to_pad_button(self) -> PadButton {
        match self {
            Self::A => PadButton::A,
            Self::B => PadButton::B,
            Self::X => PadButton::X,
            Self::Y => PadButton::Y,
            Self::Lb => PadButton::Lb,
            Self::Rb => PadButton::Rb,
            Self::Ls => PadButton::Ls,
            Self::Rs => PadButton::Rs,
            Self::Back => PadButton::Back,
            Self::Start => PadButton::Start,
            Self::Up => PadButton::Up,
            Self::Down => PadButton::Down,
            Self::Left => PadButton::Left,
            Self::Right => PadButton::Right,
            Self::Guide => PadButton::Guide,
        }
    }
}

fn validate_params(params: &ActPadParams) -> Result<(), ErrorData> {
    if params.backend == PadBackend::Hardware {
        return Err(action_error_to_mcp(&ActionError::BackendUnavailable {
            detail: "act_pad hardware backend removed; use backend=vigem".to_owned(),
        }));
    }
    if let Some(hold_ms) = params.hold_ms {
        if hold_ms == 0 {
            return Err(mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                "act_pad hold_ms must be at least 1 when provided",
            ));
        }
        if hold_ms > MAX_HOLD_MS {
            return Err(action_error_to_mcp(&ActionError::HoldExceededMax {
                detail: format!("act_pad hold_ms {hold_ms} exceeds max {MAX_HOLD_MS}"),
            }));
        }
    }
    Ok(())
}

fn validate_axis_pair(field: &'static str, value: (f32, f32)) -> Result<(), ErrorData> {
    for (axis, component) in [("x", value.0), ("y", value.1)] {
        if !(-1.0..=1.0).contains(&component) {
            return Err(mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                format!("act_pad {field}.{axis} must be in -1.0..=1.0, got {component}"),
            ));
        }
    }
    Ok(())
}

fn validate_trigger(field: &'static str, value: f32) -> Result<(), ErrorData> {
    if !(0.0..=1.0).contains(&value) {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("act_pad {field} must be in 0.0..=1.0, got {value}"),
        ));
    }
    Ok(())
}

fn execute_recording(
    recording: &RecordingBackend,
    report_action: &Action,
    hold_ms: Option<u32>,
    neutral_action: &Action,
    boundary: super::OperatorPanicActionBoundary,
) -> Result<(), ErrorData> {
    let before_events = recording.events();
    let before_event_count = before_events.len();
    let mut emit_state = EmitState::new();
    boundary.ensure("immediately_before_recorded_pad_report_dispatch")?;
    recording
        .execute(report_action, &mut emit_state)
        .map_err(|error| action_error_to_mcp(&error))?;
    if hold_ms.is_some() {
        boundary.ensure("immediately_before_recorded_pad_neutral_dispatch")?;
        recording
            .execute(neutral_action, &mut emit_state)
            .map_err(|error| action_error_to_mcp(&error))?;
    }
    let after_events = recording.events();
    let new_events = &after_events[before_event_count..];
    let event_sequence = event_sequence(new_events);
    let pad_state = recording.pad_state();
    let pad_state_label = pad_state_label(&pad_state);
    tracing::info!(
        code = "M2_ACT_PAD_RECORDING_READBACK",
        kind = "act_pad",
        before_event_count,
        after_event_count = after_events.len(),
        new_event_count = new_events.len(),
        event_sequence,
        pad_state = pad_state_label,
        ?new_events,
        "readback=recording_backend tool=act_pad after_events_readback"
    );
    Ok(())
}

fn event_sequence(events: &[RecordedInput]) -> String {
    events.iter().map(event_label).collect::<Vec<_>>().join(">")
}

fn event_label(event: &RecordedInput) -> String {
    match event {
        RecordedInput::PadReport { pad, report } => {
            format!("pad_report:pad={pad}:{}", report_label(report))
        }
        other => format!("{other:?}"),
    }
}

fn pad_state_label(pad_state: &HashMap<PadId, GamepadReport>) -> String {
    let mut entries = pad_state
        .iter()
        .map(|(pad, report)| format!("{pad}:{}", report_label(report)))
        .collect::<Vec<_>>();
    entries.sort();
    entries.join("|")
}

fn report_label(report: &GamepadReport) -> String {
    format!(
        "controller={}:buttons={}:thumb_l=({:.3},{:.3}):thumb_r=({:.3},{:.3}):lt={:.3}:rt={:.3}",
        controller_label(report.controller),
        buttons_label(&report.buttons),
        report.thumb_l.0,
        report.thumb_l.1,
        report.thumb_r.0,
        report.thumb_r.1,
        report.lt,
        report.rt
    )
}

const fn controller_label(controller: GamepadController) -> &'static str {
    match controller {
        GamepadController::X360 => "x360",
        GamepadController::Ds4 => "ds4",
    }
}

fn buttons_label(buttons: &[PadButton]) -> String {
    if buttons.is_empty() {
        return "none".to_owned();
    }
    buttons
        .iter()
        .map(|button| format!("{button:?}").to_lowercase())
        .collect::<Vec<_>>()
        .join("+")
}

const fn neutral_report(controller: ActPadController) -> GamepadReport {
    GamepadReport::neutral(controller.to_gamepad_controller())
}

const fn neutral_axis() -> (f32, f32) {
    (0.0, 0.0)
}

fn normalized_axis_pair_schema(_: &mut rmcp::schemars::SchemaGenerator) -> rmcp::schemars::Schema {
    rmcp::schemars::json_schema!({
        "type": "array",
        "prefixItems": [
            {"type": "number", "minimum": -1.0, "maximum": 1.0},
            {"type": "number", "minimum": -1.0, "maximum": 1.0}
        ],
        "minItems": 2,
        "maxItems": 2
    })
}

fn action_error_to_mcp(error: &ActionError) -> ErrorData {
    mcp_error(error.code(), error.to_string())
}

const fn default_pad_backend() -> PadBackend {
    PadBackend::Vigem
}

const fn default_pad_controller() -> ActPadController {
    ActPadController::X360
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rmcp::schemars::schema_for;
    use serde_json::Value;
    use synapse_action::{ActionEmitter, RecordedInput};
    use synapse_core::{GamepadController, PadButton};

    use super::{
        ActPadButton, ActPadController, ActPadParams, ActPadReport, PadBackend, act_pad_with_handle,
    };

    #[test]
    fn act_pad_button_schema_exposes_guide() {
        let schema = serde_json::to_value(schema_for!(ActPadButton))
            .unwrap_or_else(|error| panic!("act_pad button schema should serialize: {error}"));
        println!("readback=act_pad_schema edge=guide_button after_schema={schema}");

        assert!(
            json_contains_string(&schema, "guide"),
            "act_pad button schema must expose guide: {schema}"
        );
    }

    #[test]
    fn act_pad_button_json_maps_guide_to_core_button() {
        let button: ActPadButton = serde_json::from_str("\"guide\"")
            .unwrap_or_else(|error| panic!("guide button should deserialize: {error}"));
        let report = ActPadReport {
            buttons: vec![button],
            thumb_l: (0.0, 0.0),
            thumb_r: (0.0, 0.0),
            lt: 0.0,
            rt: 0.0,
        }
        .to_gamepad_report(ActPadController::Ds4)
        .unwrap_or_else(|error| panic!("guide report should convert: {error}"));
        println!("readback=act_pad_mapping edge=guide before=button:guide after_report={report:?}");

        assert_eq!(report.controller, GamepadController::Ds4);
        assert_eq!(report.buttons, vec![PadButton::Guide]);
    }

    #[tokio::test]
    async fn recording_backend_readback_carries_full_x360_and_ds4_reports() {
        for controller in [ActPadController::X360, ActPadController::Ds4] {
            let (handle, _snapshot_handle, _emitter) = ActionEmitter::channel();
            let recording = Arc::new(synapse_action::RecordingBackend::new());
            let params = ActPadParams {
                pad_id: match controller {
                    ActPadController::X360 => 0,
                    ActPadController::Ds4 => 1,
                },
                controller,
                report: full_sweep_report(),
                backend: PadBackend::Vigem,
                hold_ms: Some(1),
                verify_delta: false,
                verify_timeout_ms: crate::m2::default_verify_timeout_ms(),
            };
            let before = recording.events();
            println!(
                "readback=act_pad_recording edge=full_sweep before=controller:{controller:?} events={before:?}"
            );

            let response = act_pad_with_handle(handle, Some(Arc::clone(&recording)), params)
                .await
                .unwrap_or_else(|error| {
                    panic!("act_pad full sweep recording should succeed: {error}")
                });
            let after = recording.events();
            let pad_state = recording.pad_state();
            println!(
                "readback=act_pad_recording edge=full_sweep after=controller:{controller:?} response={response:?} events={after:?} pad_state={pad_state:?}"
            );

            assert!(response.ok);
            assert_eq!(response.controller, controller);
            assert!(response.returned_to_neutral);
            assert!(response.buttons.contains(&ActPadButton::Guide));
            assert_eq!(after.len(), 2);
            let first = match &after[0] {
                RecordedInput::PadReport { pad, report } => (*pad, report),
                other => panic!("first act_pad event should be PadReport, got {other:?}"),
            };
            assert_eq!(first.0, response.pad_id);
            assert_eq!(
                first.1.controller,
                match controller {
                    ActPadController::X360 => GamepadController::X360,
                    ActPadController::Ds4 => GamepadController::Ds4,
                }
            );
            assert!(first.1.buttons.contains(&PadButton::Guide));
            assert_eq!(first.1.thumb_l, (-1.0, 1.0));
            assert_eq!(first.1.thumb_r, (1.0, -1.0));
            assert_eq!(first.1.lt, 1.0);
            assert_eq!(first.1.rt, 0.5);
            assert!(pad_state.is_empty());
        }
    }

    fn full_sweep_report() -> ActPadReport {
        ActPadReport {
            buttons: vec![
                ActPadButton::A,
                ActPadButton::B,
                ActPadButton::X,
                ActPadButton::Y,
                ActPadButton::Lb,
                ActPadButton::Rb,
                ActPadButton::Ls,
                ActPadButton::Rs,
                ActPadButton::Back,
                ActPadButton::Start,
                ActPadButton::Up,
                ActPadButton::Down,
                ActPadButton::Left,
                ActPadButton::Right,
                ActPadButton::Guide,
            ],
            thumb_l: (-1.0, 1.0),
            thumb_r: (1.0, -1.0),
            lt: 1.0,
            rt: 0.5,
        }
    }

    fn json_contains_string(value: &Value, needle: &str) -> bool {
        match value {
            Value::String(value) => value == needle,
            Value::Array(values) => values
                .iter()
                .any(|value| json_contains_string(value, needle)),
            Value::Object(values) => values
                .values()
                .any(|value| json_contains_string(value, needle)),
            _ => false,
        }
    }
}
