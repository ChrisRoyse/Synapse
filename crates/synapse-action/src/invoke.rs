use synapse_core::{ElementId, MouseButton, Point};

#[cfg(not(windows))]
use crate::ActionError;
use crate::{ActionBackend, ActionResult, EmitState};

#[cfg(test)]
mod dispatch;
#[cfg(any(test, windows))]
mod resolver;

#[cfg(test)]
mod tests;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ElementClickOutcome {
    Invoked,
    Toggled {
        before_state: String,
        after_state: String,
    },
    Selected {
        was_selected: bool,
        is_selected: bool,
    },
    Expanded {
        before_state: synapse_a11y::ExpandState,
        after_state: synapse_a11y::ExpandState,
    },
    Collapsed {
        before_state: synapse_a11y::ExpandState,
        after_state: synapse_a11y::ExpandState,
    },
    LegacyDefaultAction {
        default_action: Option<String>,
    },
    CoordinateFallback(CoordinateFallbackPlan),
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct CoordinateFallbackPlan {
    pub screen_point: Point,
    pub window_point: Point,
}

/// Re-resolves a Synapse accessibility element and dispatches a semantic UIA
/// click without moving the cursor.
///
/// Supported patterns are `InvokePattern`, `TogglePattern`,
/// `SelectionItemPattern`, `ExpandCollapsePattern`, and
/// `LegacyIAccessiblePattern.DoDefaultAction`.
///
/// # Errors
///
/// On Windows, `synapse_a11y::re_resolve` failures are mapped to
/// `ACTION_ELEMENT_NOT_RESOLVED`. Missing semantic patterns are reported as
/// `ACTION_ELEMENT_PATTERN_UNSUPPORTED`; no coordinate click is attempted.
#[cfg(windows)]
pub fn invoke_element(element_id: &ElementId) -> ActionResult<()> {
    match synapse_a11y::click_element_action(element_id)
        .map_err(|error| a11y_error_to_action(element_id, error))?
    {
        synapse_a11y::ElementClickAction::Invoked
        | synapse_a11y::ElementClickAction::Toggled { .. }
        | synapse_a11y::ElementClickAction::Selected { .. }
        | synapse_a11y::ElementClickAction::Expanded { .. }
        | synapse_a11y::ElementClickAction::Collapsed { .. }
        | synapse_a11y::ElementClickAction::LegacyDefaultAction { .. } => Ok(()),
    }
}

/// Non-Windows builds expose the same API but fail closed before any action is
/// attempted.
///
/// # Errors
///
/// Always returns `ACTION_BACKEND_UNAVAILABLE` because UI Automation
/// `InvokePattern` dispatch is Windows-only.
#[cfg(not(windows))]
pub fn invoke_element(element_id: &ElementId) -> ActionResult<()> {
    Err(ActionError::BackendUnavailable {
        detail: format!("UI Automation InvokePattern requires Windows for element {element_id}"),
    })
}

/// Attempts a semantic UIA click using supported control patterns.
///
/// This path does not synthesize a coordinate fallback; unsupported patterns
/// fail with `ACTION_ELEMENT_PATTERN_UNSUPPORTED` so a higher-level router can
/// explicitly choose the next delivery tier.
///
/// # Errors
///
/// Returns `ACTION_ELEMENT_NOT_RESOLVED` when UIA re-resolution fails,
/// `ACTION_ELEMENT_PATTERN_UNSUPPORTED` when the element exposes no supported
/// click pattern, or a backend-specific action error if UIA dispatch fails.
#[cfg(windows)]
pub fn click_element_or_fallback<B>(
    element_id: &ElementId,
    _backend: &B,
    _state: &mut EmitState,
    _button: MouseButton,
) -> ActionResult<ElementClickOutcome>
where
    B: ActionBackend,
{
    match synapse_a11y::click_element_action(element_id)
        .map_err(|error| a11y_error_to_action(element_id, error))?
    {
        synapse_a11y::ElementClickAction::Invoked => Ok(ElementClickOutcome::Invoked),
        synapse_a11y::ElementClickAction::Toggled {
            before_state,
            after_state,
        } => Ok(ElementClickOutcome::Toggled {
            before_state,
            after_state,
        }),
        synapse_a11y::ElementClickAction::Selected {
            was_selected,
            is_selected,
        } => Ok(ElementClickOutcome::Selected {
            was_selected,
            is_selected,
        }),
        synapse_a11y::ElementClickAction::Expanded {
            before_state,
            after_state,
        } => Ok(ElementClickOutcome::Expanded {
            before_state,
            after_state,
        }),
        synapse_a11y::ElementClickAction::Collapsed {
            before_state,
            after_state,
        } => Ok(ElementClickOutcome::Collapsed {
            before_state,
            after_state,
        }),
        synapse_a11y::ElementClickAction::LegacyDefaultAction { default_action } => {
            Ok(ElementClickOutcome::LegacyDefaultAction { default_action })
        }
    }
}

/// Non-Windows builds expose the same API but fail closed before any action is
/// attempted.
///
/// # Errors
///
/// Always returns `ACTION_BACKEND_UNAVAILABLE` because UI Automation
/// `InvokePattern` dispatch and bounding-rectangle fallback are Windows-only.
#[cfg(not(windows))]
pub fn click_element_or_fallback<B>(
    element_id: &ElementId,
    _backend: &B,
    _state: &mut EmitState,
    _button: MouseButton,
) -> ActionResult<ElementClickOutcome>
where
    B: ActionBackend,
{
    Err(ActionError::BackendUnavailable {
        detail: format!("UI Automation element click requires Windows for element {element_id}"),
    })
}

#[cfg(windows)]
#[expect(
    dead_code,
    reason = "reserved for element-target action paths that need screen-point readback"
)]
pub(crate) fn element_screen_point(element_id: &ElementId) -> ActionResult<Point> {
    let rect = synapse_a11y::element_bounding_rect(element_id)
        .map_err(|error| a11y_error_to_action(element_id, error))?;
    resolver::coordinate_fallback_plan(element_id, rect).map(|plan| plan.screen_point)
}

#[cfg(not(windows))]
#[expect(
    dead_code,
    reason = "reserved for element-target action paths that need screen-point readback"
)]
pub(crate) fn element_screen_point(element_id: &ElementId) -> ActionResult<Point> {
    Err(ActionError::BackendUnavailable {
        detail: format!("UI Automation element target requires Windows for element {element_id}"),
    })
}

#[cfg(windows)]
fn a11y_error_to_action(
    element_id: &ElementId,
    error: synapse_a11y::A11yError,
) -> crate::ActionError {
    let detail = error.to_string();
    if stale_provider_failure(&detail) {
        return resolver::transient_element_expired(element_id, detail);
    }
    match error {
        synapse_a11y::A11yError::ElementStale { .. } => {
            resolver::transient_element_expired(element_id, detail)
        }
        synapse_a11y::A11yError::ElementPatternUnsupported { .. }
        | synapse_a11y::A11yError::ElementValueUnsupported { .. } => {
            resolver::element_pattern_unsupported(element_id, detail)
        }
        synapse_a11y::A11yError::ElementValueReadOnly { .. }
        | synapse_a11y::A11yError::ElementNotEnabled { .. }
        | synapse_a11y::A11yError::CdpUnreachable { .. }
        | synapse_a11y::A11yError::CdpAttachFailed { .. }
        | synapse_a11y::A11yError::CdpAxtreeFailed { .. }
        | synapse_a11y::A11yError::BrowserWaitTimeout { .. }
        | synapse_a11y::A11yError::CdpEvaluateTimeout { .. }
        | synapse_a11y::A11yError::Internal { .. } => resolver::target_invalid(detail),
        synapse_a11y::A11yError::InvalidElementId { .. }
        | synapse_a11y::A11yError::NoForeground { .. } => resolver::element_not_resolved(detail),
        synapse_a11y::A11yError::NotAvailable { detail } => {
            crate::ActionError::BackendUnavailable { detail }
        }
        synapse_a11y::A11yError::UiaWorkerTimeout { .. } => {
            crate::ActionError::BackendUnavailable { detail }
        }
        synapse_a11y::A11yError::ForegroundActivationRefused { .. } => {
            crate::ActionError::ForegroundActivationRefused { detail }
        }
    }
}

#[cfg(windows)]
fn stale_provider_failure(detail: &str) -> bool {
    let detail = detail.to_ascii_lowercase();
    detail.contains("event was unable to invoke any of the subscribers")
        || detail.contains("element not available")
        || detail.contains("element is no longer available")
        || detail.contains("uia_e_elementnotavailable")
}
