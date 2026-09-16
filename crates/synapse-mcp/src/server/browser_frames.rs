//! Browser frame enumeration tools (#1183).

use super::{
    ErrorData, Json, Parameters, SynapseService, TargetWire,
    m1_tools::{
        browser_raw_cdp_required_error, cdp_target_id_audit_ref, require_target_session_id,
    },
    tool, tool_router,
};
use crate::m1::mcp_error;
use crate::server::url_redaction::redact_url_for_public_readback;
use rmcp::{RoleServer, schemars::JsonSchema, service::RequestContext};
use serde::{Deserialize, Serialize};
use serde_json::json;

const FRAMES_TOOL: &str = "browser_frames";
const CHROME_TAB_PREFIX: &str = "chrome-tab:";

#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserFramesParams {
    /// CDP TargetID to enumerate. Defaults to the active session CDP target. Must
    /// be owned by this session; the human foreground tab is never a fallback.
    #[serde(default)]
    pub cdp_target_id: Option<String>,
    /// Browser HWND that owns the target. Required only with an explicit
    /// `cdp_target_id` and no active session target.
    #[serde(default)]
    #[schemars(range(min = 1, max = 4_294_967_295_u64))]
    pub window_hwnd: Option<i64>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserFrameEntry {
    pub frame_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_frame_id: Option<String>,
    pub target: TargetWire,
    pub cdp_target_id: String,
    pub target_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_attached: Option<bool>,
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub origin: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub security_origin: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loader_id: Option<String>,
    pub depth: u32,
    pub sibling_index: u32,
    pub child_count: u32,
    pub is_out_of_process: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame_element_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame_element_backend_node_id: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame_element_cdp_target_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame_element_target: Option<TargetWire>,
    pub frame_element_source: String,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BrowserFramesResponse {
    pub session_id: String,
    pub window_hwnd: i64,
    pub transport: String,
    pub endpoint: String,
    pub cdp_target_id: String,
    pub page_url: String,
    pub page_title: String,
    pub frame_count: usize,
    pub oopif_target_count: u32,
    pub attached_frame_target_count: u32,
    pub frames: Vec<BrowserFrameEntry>,
    pub blocked_frame_targets: Vec<String>,
    pub frame_snapshot_errors: Vec<String>,
    pub readback_backend: String,
    pub backend_tier_used: String,
    pub required_foreground: bool,
    pub source_of_truth: String,
}

#[tool_router(router = browser_frames_tool_router, vis = "pub(super)")]
impl SynapseService {
    #[tool(
        description = "Enumerate the composed frame tree for the calling session's owned browser tab. Raw CDP returns stable Page.FrameId values, parent frame ids, target ids, frame URLs, names, origins, nesting depth, sibling order, OOPIF target metadata, and owning iframe/frame element ids when Chromium exposes DOM.Node.frameId. The normal Chrome bridge supports chrome-tab:* targets through debugger-free chrome.webNavigation.getAllFrames plus optional chrome.scripting metadata, with frame owner element ids unavailable. Target-scoped and background-safe: never activates the tab, never uses OS foreground input, and never falls back to the human foreground tab."
    )]
    pub async fn browser_frames(
        &self,
        params: Parameters<BrowserFramesParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<BrowserFramesResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = FRAMES_TOOL,
            "tool.invocation kind=browser_frames"
        );
        let session_id = require_target_session_id(&request_context)?;
        let request_details = json!({
            "session_id": &session_id,
            "window_hwnd": params.0.window_hwnd,
            "requested_cdp_target": cdp_target_id_audit_ref(params.0.cdp_target_id.as_deref()),
            "required_foreground": false,
            "phase": "target_resolution",
        });
        let resolution = self.resolve_cdp_tab_mutation_target(
            FRAMES_TOOL,
            &session_id,
            params.0.window_hwnd,
            params.0.cdp_target_id.as_deref(),
        );
        let (window_hwnd, cdp_target_id) = self.audit_cdp_target_resolution_result(
            FRAMES_TOOL,
            &session_id,
            &request_details,
            resolution,
        )?;
        let request_details = json!({
            "session_id": &session_id,
            "window_hwnd": window_hwnd,
            "cdp_target_id": &cdp_target_id,
            "required_foreground": false,
        });
        self.audit_action_started_with_details_for_session(
            FRAMES_TOOL,
            &request_details,
            &session_id,
        )?;
        let result = self
            .browser_frames_impl(&session_id, window_hwnd, &cdp_target_id)
            .await;
        self.audit_action_result_for_session(FRAMES_TOOL, &result, &session_id)?;
        result.map(Json)
    }

    #[cfg(windows)]
    async fn browser_frames_impl(
        &self,
        session_id: &str,
        window_hwnd: i64,
        cdp_target_id: &str,
    ) -> Result<BrowserFramesResponse, ErrorData> {
        let Some(endpoint) = synapse_a11y::endpoint_for_window(window_hwnd) else {
            if cdp_target_id.starts_with(CHROME_TAB_PREFIX) {
                let result = crate::chrome_debugger_bridge::frames(window_hwnd, cdp_target_id)
                    .await
                    .map_err(|error| {
                        mcp_error(
                            error.code(),
                            format!(
                                "{FRAMES_TOOL} normal bridge frame enumeration failed for target {cdp_target_id:?}: {}",
                                error.detail()
                            ),
                        )
                    })?;
                tracing::info!(
                    code = "CHROME_BRIDGE_BACKGROUND_FRAME_ENUMERATION",
                    session_id = %session_id,
                    hwnd = window_hwnd,
                    cdp_target_id = %result.target_id,
                    frame_count = result.frame_count,
                    oopif_target_count = result.oopif_target_count,
                    attached_frame_target_count = result.attached_frame_target_count,
                    target_url = %result.url,
                    "readback=chrome.webNavigation.getAllFrames+chrome.scripting.executeScript outcome=frames_returned"
                );
                return Ok(BrowserFramesResponse {
                    session_id: session_id.to_owned(),
                    window_hwnd,
                    transport: "chrome_tabs_extension".to_owned(),
                    endpoint: "chrome-extension://leoocgnkjnplbfdbklajepahofecgfbk/chrome.webNavigation".to_owned(),
                    cdp_target_id: result.target_id,
                    page_url: redact_url_for_public_readback(&result.url),
                    page_title: result.title,
                    frame_count: result.frame_count,
                    oopif_target_count: result.oopif_target_count,
                    attached_frame_target_count: result.attached_frame_target_count,
                    frames: result
                        .frames
                        .into_iter()
                        .map(|frame| browser_bridge_frame_entry(window_hwnd, frame))
                        .collect(),
                    blocked_frame_targets: result.blocked_frame_targets,
                    frame_snapshot_errors: result.frame_snapshot_errors,
                    readback_backend: result.readback_backend,
                    backend_tier_used: result.backend_tier_used,
                    required_foreground: false,
                    source_of_truth:
                        "normal Chrome bridge chrome.webNavigation.getAllFrames plus content-script frame metadata"
                            .to_owned(),
                });
            }
            return Err(browser_raw_cdp_required_error(FRAMES_TOOL, window_hwnd));
        };
        let result = synapse_a11y::cdp_list_frames(&endpoint, window_hwnd, cdp_target_id)
            .await
            .map_err(|error| {
                mcp_error(
                    error.code(),
                    format!("{FRAMES_TOOL} raw CDP frame enumeration failed: {error}"),
                )
            })?;
        tracing::info!(
            code = "CDP_BACKGROUND_FRAME_ENUMERATION",
            session_id = %session_id,
            hwnd = window_hwnd,
            endpoint = %result.endpoint,
            cdp_target_id = %result.target_id,
            frame_count = result.frame_count,
            oopif_target_count = result.oopif_target_count,
            attached_frame_target_count = result.attached_frame_target_count,
            "readback=Page.getFrameTree+DOM.getDocument+Target.getTargets outcome=frames_returned"
        );
        Ok(BrowserFramesResponse {
            session_id: session_id.to_owned(),
            window_hwnd,
            transport: "raw_cdp".to_owned(),
            endpoint: result.endpoint,
            cdp_target_id: result.target_id,
            page_url: redact_url_for_public_readback(&result.page_url),
            page_title: result.page_title,
            frame_count: result.frame_count,
            oopif_target_count: result.oopif_target_count,
            attached_frame_target_count: result.attached_frame_target_count,
            frames: result
                .frames
                .into_iter()
                .map(|frame| browser_frame_entry(window_hwnd, frame))
                .collect(),
            blocked_frame_targets: result.blocked_frame_targets,
            frame_snapshot_errors: result.frame_snapshot_errors,
            readback_backend: "Page.getFrameTree + DOM.getDocument + Target.getTargets".to_owned(),
            backend_tier_used: "cdp".to_owned(),
            required_foreground: false,
            source_of_truth: "raw CDP Page.getFrameTree plus DOM frame-owner nodes".to_owned(),
        })
    }

    #[cfg(not(windows))]
    async fn browser_frames_impl(
        &self,
        _session_id: &str,
        _window_hwnd: i64,
        _cdp_target_id: &str,
    ) -> Result<BrowserFramesResponse, ErrorData> {
        Err(mcp_error(
            synapse_core::error_codes::A11Y_NOT_AVAILABLE,
            "browser_frames is only available on Windows in this build",
        ))
    }
}

fn browser_frame_entry(
    window_hwnd: i64,
    frame: synapse_a11y::CdpFrameTreeEntry,
) -> BrowserFrameEntry {
    let frame_element_target =
        frame
            .frame_element_cdp_target_id
            .as_ref()
            .map(|target_id| TargetWire::Cdp {
                window_hwnd,
                cdp_target_id: target_id.clone(),
            });
    BrowserFrameEntry {
        frame_id: frame.frame_id,
        parent_frame_id: frame.parent_frame_id,
        target: TargetWire::Cdp {
            window_hwnd,
            cdp_target_id: frame.cdp_target_id.clone(),
        },
        cdp_target_id: frame.cdp_target_id,
        target_type: frame.target_type,
        target_attached: frame.target_attached,
        url: redact_url_for_public_readback(&frame.url),
        name: frame.name,
        origin: frame.origin,
        security_origin: frame.security_origin,
        loader_id: frame.loader_id,
        depth: frame.depth,
        sibling_index: frame.sibling_index,
        child_count: frame.child_count,
        is_out_of_process: frame.is_out_of_process,
        frame_element_id: frame.frame_element_id,
        frame_element_backend_node_id: frame.frame_element_backend_node_id,
        frame_element_cdp_target_id: frame.frame_element_cdp_target_id,
        frame_element_target,
        frame_element_source: frame.frame_element_source,
    }
}

#[cfg(windows)]
fn browser_bridge_frame_entry(
    window_hwnd: i64,
    frame: crate::chrome_debugger_bridge::ChromeDebuggerFrameEntry,
) -> BrowserFrameEntry {
    let frame_element_target =
        frame
            .frame_element_cdp_target_id
            .as_ref()
            .map(|target_id| TargetWire::Cdp {
                window_hwnd,
                cdp_target_id: target_id.clone(),
            });
    BrowserFrameEntry {
        frame_id: frame.frame_id,
        parent_frame_id: frame.parent_frame_id,
        target: TargetWire::Cdp {
            window_hwnd,
            cdp_target_id: frame.cdp_target_id.clone(),
        },
        cdp_target_id: frame.cdp_target_id,
        target_type: frame.target_type,
        target_attached: frame.target_attached,
        url: redact_url_for_public_readback(&frame.url),
        name: frame.name,
        origin: frame.origin,
        security_origin: frame.security_origin,
        loader_id: frame.loader_id,
        depth: frame.depth,
        sibling_index: frame.sibling_index,
        child_count: frame.child_count,
        is_out_of_process: frame.is_out_of_process,
        frame_element_id: frame.frame_element_id,
        frame_element_backend_node_id: frame.frame_element_backend_node_id,
        frame_element_cdp_target_id: frame.frame_element_cdp_target_id,
        frame_element_target,
        frame_element_source: frame.frame_element_source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browser_frame_entry_maps_document_and_owner_targets() {
        let frame = synapse_a11y::CdpFrameTreeEntry {
            frame_id: "child-frame".to_owned(),
            parent_frame_id: Some("root-frame".to_owned()),
            cdp_target_id: "iframe-target".to_owned(),
            target_type: "iframe".to_owned(),
            target_attached: Some(true),
            url: "https://child.example/".to_owned(),
            name: Some("child".to_owned()),
            origin: "https://child.example".to_owned(),
            security_origin: Some("https://child.example".to_owned()),
            loader_id: Some("loader-1".to_owned()),
            depth: 1,
            sibling_index: 2,
            child_count: 0,
            is_out_of_process: true,
            frame_element_id: Some("0000000000002200:cdcd00000000002a".to_owned()),
            frame_element_backend_node_id: Some(42),
            frame_element_cdp_target_id: Some("root-target".to_owned()),
            frame_element_source: "DOM.Node.frameId".to_owned(),
        };

        let wire = browser_frame_entry(0x2200, frame);

        assert_eq!(wire.frame_id, "child-frame");
        assert_eq!(wire.cdp_target_id, "iframe-target");
        assert!(wire.is_out_of_process);
        match wire.target {
            TargetWire::Cdp {
                window_hwnd,
                cdp_target_id,
            } => {
                assert_eq!(window_hwnd, 0x2200);
                assert_eq!(cdp_target_id, "iframe-target");
            }
            TargetWire::Window { .. } => panic!("expected CDP frame target"),
        }
        match wire.frame_element_target {
            Some(TargetWire::Cdp {
                window_hwnd,
                cdp_target_id,
            }) => {
                assert_eq!(window_hwnd, 0x2200);
                assert_eq!(cdp_target_id, "root-target");
            }
            other => panic!("expected CDP owner target, got {other:?}"),
        }
    }

    #[cfg(windows)]
    #[test]
    fn browser_bridge_frame_entry_maps_normal_tab_frame() {
        let frame = crate::chrome_debugger_bridge::ChromeDebuggerFrameEntry {
            frame_id: "2".to_owned(),
            parent_frame_id: Some("0".to_owned()),
            cdp_target_id: "chrome-tab:44".to_owned(),
            target_type: "iframe".to_owned(),
            target_attached: Some(false),
            url: "http://localhost:7700/health".to_owned(),
            name: Some("health-frame".to_owned()),
            origin: "http://localhost:7700".to_owned(),
            security_origin: Some("http://localhost:7700".to_owned()),
            loader_id: None,
            depth: 1,
            sibling_index: 0,
            child_count: 0,
            is_out_of_process: false,
            frame_element_id: None,
            frame_element_backend_node_id: None,
            frame_element_cdp_target_id: None,
            frame_element_source: "chrome.webNavigation.getAllFrames".to_owned(),
        };

        let wire = browser_bridge_frame_entry(0x3300, frame);

        assert_eq!(wire.frame_id, "2");
        assert_eq!(wire.parent_frame_id.as_deref(), Some("0"));
        assert_eq!(wire.cdp_target_id, "chrome-tab:44");
        assert_eq!(wire.depth, 1);
        assert_eq!(
            wire.frame_element_source,
            "chrome.webNavigation.getAllFrames"
        );
        match wire.target {
            TargetWire::Cdp {
                window_hwnd,
                cdp_target_id,
            } => {
                assert_eq!(window_hwnd, 0x3300);
                assert_eq!(cdp_target_id, "chrome-tab:44");
            }
            TargetWire::Window { .. } => panic!("expected CDP frame target"),
        }
    }
}
