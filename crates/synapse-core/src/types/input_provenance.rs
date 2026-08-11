//! Typed input-delivery provenance shared by every public input surface (#2167).
//!
//! DOM trust, browser default actions, transport, OS foreground ownership, and
//! physical-device origin are independent facts.  This record keeps them
//! independent and versioned so callers and durable audit rows cannot turn a
//! protocol-generated event into a claim of physical input.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub const INPUT_PROVENANCE_SCHEMA_VERSION: &str = "synapse.input_provenance.v1";

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InputDeliveryOrigin {
    DomDispatch,
    HtmlActivationMethod,
    HtmlElementMethod,
    CdpProtocol,
    ChromeDebuggerProtocol,
    UiaPattern,
    Win32PostMessage,
    OsSendInput,
    VirtualHid,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PhysicalDeviceOrigin {
    False,
    Unknown,
    True,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BrowserDefaultActionSemantics {
    UserAgentInput,
    HtmlActivationBehavior,
    HtmlElementMethodEffects,
    SyntheticDispatchNoUserAgentInputDefaults,
    SyntheticClickMayRunActivationBehavior,
    ScriptedMutationPlusSyntheticNotifications,
    UiaProviderDefined,
    Win32MessageProcessing,
    OsInputPipeline,
    NotApplicable,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InputTargetKind {
    BrowserTab,
    NativeWindow,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct InputTargetIdentity {
    pub session_id: String,
    pub kind: InputTargetKind,
    pub window_hwnd: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cdp_target_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct InputForegroundSnapshot {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hwnd: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_owned: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_error: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct InputForegroundProvenance {
    pub required: bool,
    pub target_hwnd: i64,
    pub before: InputForegroundSnapshot,
    pub after: InputForegroundSnapshot,
    pub per_emission_fence_verified: bool,
    pub source_of_truth: String,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct InputProvenance {
    pub schema_version: String,
    pub delivery_origin: InputDeliveryOrigin,
    /// Expected `Event.isTrusted` for DOM events produced by this lane. `None`
    /// means the target is not a browser DOM event surface.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_dom_event_is_trusted: Option<bool>,
    pub physical_device_origin: PhysicalDeviceOrigin,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub physical_device_evidence: Option<String>,
    pub browser_default_actions: BrowserDefaultActionSemantics,
    pub backend: String,
    pub transport: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol_method: Option<String>,
    pub target: InputTargetIdentity,
    pub foreground: InputForegroundProvenance,
}

impl InputProvenance {
    /// Validates cross-field claims before the record can be returned or
    /// persisted. This rejects contradictions instead of picking a friendlier
    /// interpretation of the selected input lane.
    ///
    /// # Errors
    ///
    /// Returns a precise contradiction when the schema version, target,
    /// physical-origin evidence, delivery semantics, or foreground evidence do
    /// not form one internally consistent provenance record.
    pub fn validate(&self) -> Result<(), String> {
        if self.schema_version != INPUT_PROVENANCE_SCHEMA_VERSION {
            return Err(format!(
                "schema_version {:?} must equal {:?}",
                self.schema_version, INPUT_PROVENANCE_SCHEMA_VERSION
            ));
        }
        self.validate_target()?;
        self.validate_physical_origin()?;
        self.validate_delivery_origin()?;
        self.validate_foreground()?;
        Ok(())
    }

    fn validate_target(&self) -> Result<(), String> {
        if self.target.session_id.trim().is_empty() {
            return Err("target.session_id must be non-empty".to_owned());
        }
        if self.target.window_hwnd <= 0 || self.foreground.target_hwnd <= 0 {
            return Err("target and foreground HWND values must be positive".to_owned());
        }
        if self.target.window_hwnd != self.foreground.target_hwnd {
            return Err(format!(
                "target.window_hwnd {} contradicts foreground.target_hwnd {}",
                self.target.window_hwnd, self.foreground.target_hwnd
            ));
        }
        match self.target.kind {
            InputTargetKind::BrowserTab => {
                if self
                    .target
                    .cdp_target_id
                    .as_deref()
                    .is_none_or(|value| value.trim().is_empty())
                {
                    return Err("browser_tab target requires cdp_target_id".to_owned());
                }
                if self.expected_dom_event_is_trusted.is_none() {
                    return Err(
                        "browser_tab input requires expected_dom_event_is_trusted".to_owned()
                    );
                }
            }
            InputTargetKind::NativeWindow => {
                if self.target.cdp_target_id.is_some() {
                    return Err("native_window target must not carry cdp_target_id".to_owned());
                }
                if self.expected_dom_event_is_trusted.is_some() {
                    return Err(
                        "native_window input must not carry expected_dom_event_is_trusted"
                            .to_owned(),
                    );
                }
            }
        }
        if self.backend.trim().is_empty() || self.transport.trim().is_empty() {
            return Err("backend and transport must be non-empty".to_owned());
        }
        if self.foreground.source_of_truth.trim().is_empty() {
            return Err("foreground.source_of_truth must be non-empty".to_owned());
        }
        Ok(())
    }

    fn validate_physical_origin(&self) -> Result<(), String> {
        if self.physical_device_origin == PhysicalDeviceOrigin::True
            && self
                .physical_device_evidence
                .as_deref()
                .is_none_or(|value| value.trim().is_empty())
        {
            return Err(
                "physical_device_origin=true requires non-empty physical_device_evidence"
                    .to_owned(),
            );
        }
        if self.physical_device_origin == PhysicalDeviceOrigin::False
            && self.physical_device_evidence.is_some()
        {
            return Err(
                "physical_device_origin=false contradicts physical_device_evidence".to_owned(),
            );
        }
        Ok(())
    }

    fn validate_delivery_origin(&self) -> Result<(), String> {
        if self.delivery_origin == InputDeliveryOrigin::DomDispatch
            && self.expected_dom_event_is_trusted != Some(false)
        {
            return Err("DOM dispatch requires expected_dom_event_is_trusted=false".to_owned());
        }
        if matches!(
            self.delivery_origin,
            InputDeliveryOrigin::CdpProtocol | InputDeliveryOrigin::ChromeDebuggerProtocol
        ) {
            if self.target.kind != InputTargetKind::BrowserTab {
                return Err("CDP/chrome.debugger input requires a browser_tab target".to_owned());
            }
            if self.expected_dom_event_is_trusted != Some(true) {
                return Err(
                    "CDP/chrome.debugger input requires expected_dom_event_is_trusted=true"
                        .to_owned(),
                );
            }
            if self.physical_device_origin != PhysicalDeviceOrigin::False {
                return Err(
                    "CDP/chrome.debugger protocol input must report physical_device_origin=false"
                        .to_owned(),
                );
            }
            if self.browser_default_actions != BrowserDefaultActionSemantics::UserAgentInput {
                return Err(
                    "CDP/chrome.debugger input requires browser_default_actions=user_agent_input"
                        .to_owned(),
                );
            }
        }
        if matches!(
            self.delivery_origin,
            InputDeliveryOrigin::DomDispatch
                | InputDeliveryOrigin::HtmlActivationMethod
                | InputDeliveryOrigin::HtmlElementMethod
        ) && self.target.kind != InputTargetKind::BrowserTab
        {
            return Err("DOM/HTML input origins require a browser_tab target".to_owned());
        }
        if matches!(
            self.delivery_origin,
            InputDeliveryOrigin::UiaPattern | InputDeliveryOrigin::Win32PostMessage
        ) && self.target.kind != InputTargetKind::NativeWindow
        {
            return Err(
                "UIA/Win32 message input origins require a native_window target".to_owned(),
            );
        }
        Ok(())
    }

    fn validate_foreground(&self) -> Result<(), String> {
        if self.foreground.required != self.foreground.per_emission_fence_verified {
            return Err(
                "foreground.required must equal per_emission_fence_verified for current delivery lanes"
                    .to_owned(),
            );
        }
        if self.foreground.required && self.foreground.before.target_owned != Some(true) {
            return Err("foreground-required input requires before.target_owned=true".to_owned());
        }
        Ok(())
    }
}
