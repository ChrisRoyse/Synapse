//! One fail-closed input-provenance constructor for public input surfaces (#2167).

use rmcp::{ErrorData, model::ErrorCode};
use serde_json::json;
use synapse_core::{
    BrowserDefaultActionSemantics, InputDeliveryOrigin, InputForegroundProvenance,
    InputForegroundSnapshot, InputProvenance, InputTargetIdentity, InputTargetKind,
    PhysicalDeviceOrigin, error_codes,
};

use crate::m1::mcp_error;

const FOREGROUND_SOURCE_OF_TRUTH: &str = "GetForegroundWindow + GetWindowThreadProcessId read before and after the input transaction; foreground-required emissions additionally use the per-emission foreground fence";

#[derive(Clone, Debug)]
pub(crate) struct InputProvenanceContext {
    target: InputTargetIdentity,
    before: InputForegroundSnapshot,
    raw_cdp_endpoint_present: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct InputProvenanceSpec<'a> {
    pub delivery_origin: InputDeliveryOrigin,
    pub expected_dom_event_is_trusted: Option<bool>,
    pub browser_default_actions: BrowserDefaultActionSemantics,
    pub backend: &'a str,
    pub transport: &'a str,
    pub protocol_method: Option<&'a str>,
    pub required_foreground: bool,
    pub per_emission_fence_verified: bool,
}

impl InputProvenanceContext {
    pub(crate) const fn target(&self) -> &InputTargetIdentity {
        &self.target
    }

    pub(crate) const fn raw_cdp_endpoint_present(&self) -> bool {
        self.raw_cdp_endpoint_present
    }

    pub(crate) fn browser_tab(
        session_id: &str,
        window_hwnd: i64,
        cdp_target_id: &str,
    ) -> Result<Self, ErrorData> {
        Self::capture(InputTargetIdentity {
            session_id: session_id.to_owned(),
            kind: InputTargetKind::BrowserTab,
            window_hwnd,
            cdp_target_id: Some(cdp_target_id.to_owned()),
        })
    }

    pub(crate) fn native_window(session_id: &str, window_hwnd: i64) -> Result<Self, ErrorData> {
        Self::capture(InputTargetIdentity {
            session_id: session_id.to_owned(),
            kind: InputTargetKind::NativeWindow,
            window_hwnd,
            cdp_target_id: None,
        })
    }

    fn capture(target: InputTargetIdentity) -> Result<Self, ErrorData> {
        if target.window_hwnd <= 0 {
            return Err(input_provenance_error(
                "capture",
                format!(
                    "input target HWND must be positive, got {}",
                    target.window_hwnd
                ),
                Some(&target),
            ));
        }
        let before = foreground_snapshot(target.window_hwnd);
        let raw_cdp_endpoint_present = target.kind == InputTargetKind::BrowserTab
            && synapse_a11y::endpoint_for_window(target.window_hwnd).is_some();
        Ok(Self {
            target,
            before,
            raw_cdp_endpoint_present,
        })
    }

    pub(crate) fn finish(
        &self,
        spec: InputProvenanceSpec<'_>,
    ) -> Result<InputProvenance, ErrorData> {
        let after = foreground_snapshot(self.target.window_hwnd);
        if spec.required_foreground
            && spec.per_emission_fence_verified
            && self.before.target_owned != Some(true)
        {
            return Err(input_provenance_error(
                "foreground_before",
                format!(
                    "foreground-required input claims a verified per-emission fence, but the independent pre-transaction foreground readback did not own target HWND {:#x}: {:?}",
                    self.target.window_hwnd, self.before
                ),
                Some(&self.target),
            ));
        }
        let provenance = InputProvenance {
            schema_version: synapse_core::INPUT_PROVENANCE_SCHEMA_VERSION.to_owned(),
            delivery_origin: spec.delivery_origin,
            expected_dom_event_is_trusted: spec.expected_dom_event_is_trusted,
            // Every current Synapse delivery lane is software-generated. A
            // future physical sensor may set True only with explicit evidence.
            physical_device_origin: PhysicalDeviceOrigin::False,
            physical_device_evidence: None,
            browser_default_actions: spec.browser_default_actions,
            backend: spec.backend.to_owned(),
            transport: spec.transport.to_owned(),
            protocol_method: spec.protocol_method.map(str::to_owned),
            target: self.target.clone(),
            foreground: InputForegroundProvenance {
                required: spec.required_foreground,
                target_hwnd: self.target.window_hwnd,
                before: self.before.clone(),
                after,
                per_emission_fence_verified: spec.per_emission_fence_verified,
                source_of_truth: FOREGROUND_SOURCE_OF_TRUTH.to_owned(),
            },
        };
        provenance.validate().map_err(|detail| {
            input_provenance_error("cross_field_validation", detail, Some(&self.target))
        })?;
        Ok(provenance)
    }
}

fn foreground_snapshot(target_hwnd: i64) -> InputForegroundSnapshot {
    #[cfg(windows)]
    {
        match synapse_a11y::current_foreground_context() {
            Ok(context) => InputForegroundSnapshot {
                hwnd: Some(context.hwnd),
                pid: Some(context.pid),
                target_owned: Some(context.hwnd == target_hwnd),
                read_error: None,
            },
            Err(error) => InputForegroundSnapshot {
                hwnd: None,
                pid: None,
                target_owned: None,
                read_error: Some(format!("{}: {error}", error.code())),
            },
        }
    }
    #[cfg(not(windows))]
    {
        let _ = target_hwnd;
        InputForegroundSnapshot {
            hwnd: None,
            pid: None,
            target_owned: None,
            read_error: Some("Windows GetForegroundWindow readback is unavailable".to_owned()),
        }
    }
}

pub(crate) fn input_provenance_error(
    stage: &'static str,
    detail: impl Into<String>,
    target: Option<&InputTargetIdentity>,
) -> ErrorData {
    let detail = detail.into();
    tracing::error!(
        code = error_codes::ACTION_INPUT_PROVENANCE_INVALID,
        stage,
        detail = %detail,
        target = ?target,
        "input-producing operation refused an absent or contradictory provenance record"
    );
    ErrorData::new(
        ErrorCode(-32099),
        format!("input provenance invalid at {stage}: {detail}"),
        Some(json!({
            "code": error_codes::ACTION_INPUT_PROVENANCE_INVALID,
            "detail_code": "INPUT_PROVENANCE_CONTRACT_INVALID",
            "stage": stage,
            "detail": detail,
            "target": target,
            "source_of_truth": "typed synapse.input_provenance.v1 validation before response/audit persistence",
            "remediation": "fix the selected input lane to emit one internally consistent typed provenance record; do not omit fields or relabel the lane",
        })),
    )
}

pub(crate) fn result_string<'a>(value: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(serde_json::Value::as_str)
}

pub(crate) fn result_bool(value: &serde_json::Value, key: &str) -> Option<bool> {
    value.get(key).and_then(serde_json::Value::as_bool)
}

pub(crate) fn required_result_string<'a>(
    value: &'a serde_json::Value,
    key: &str,
    stage: &'static str,
) -> Result<&'a str, ErrorData> {
    result_string(value, key)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            input_provenance_error(
                stage,
                format!(
                    "delegated result omitted non-empty string {key:?}; fix that lane to return its exact backend contract"
                ),
                None,
            )
        })
}

pub(crate) fn required_any_result_string<'a>(
    value: &'a serde_json::Value,
    keys: &[&str],
    stage: &'static str,
) -> Result<&'a str, ErrorData> {
    keys.iter()
        .find_map(|key| result_string(value, key).filter(|candidate| !candidate.trim().is_empty()))
        .ok_or_else(|| {
            input_provenance_error(
                stage,
                format!(
                    "delegated result omitted every exact backend field {keys:?}; fix that lane to return its selected backend/method"
                ),
                None,
            )
        })
}

pub(crate) fn embedded_input_provenance(
    value: &serde_json::Value,
    key: &str,
    stage: &'static str,
    context: &InputProvenanceContext,
) -> Result<InputProvenance, ErrorData> {
    let raw = value.get(key).ok_or_else(|| {
        input_provenance_error(
            stage,
            format!("delegated result omitted typed {key:?}"),
            Some(context.target()),
        )
    })?;
    let provenance: InputProvenance = serde_json::from_value(raw.clone()).map_err(|error| {
        input_provenance_error(
            stage,
            format!("delegated {key:?} did not decode as synapse.input_provenance.v1: {error}"),
            Some(context.target()),
        )
    })?;
    provenance
        .validate()
        .map_err(|detail| input_provenance_error(stage, detail, Some(context.target())))?;
    if provenance.target != *context.target() {
        return Err(input_provenance_error(
            stage,
            format!(
                "delegated provenance target {:?} contradicts act target {:?}",
                provenance.target,
                context.target()
            ),
            Some(context.target()),
        ));
    }
    Ok(provenance)
}

pub(crate) fn required_result_bool(
    value: &serde_json::Value,
    key: &str,
    stage: &'static str,
) -> Result<bool, ErrorData> {
    result_bool(value, key).ok_or_else(|| {
        mcp_error(
            error_codes::ACTION_INPUT_PROVENANCE_INVALID,
            format!(
                "input provenance invalid at {stage}: delegated result omitted boolean {key:?}; remediation=fix the selected input lane to return its exact input contract"
            ),
        )
    })
}
