use super::{
    BrowserAddInitScriptParams, BrowserAddInitScriptResponse, BrowserAddScriptTagParams,
    BrowserAddStyleTagParams, BrowserAddTagResponse, BrowserAdoptActiveTabParams,
    BrowserAdoptActiveTabResponse, BrowserConsoleMessagesParams, BrowserConsoleMessagesResponse,
    BrowserContentParams, BrowserContentResponse, BrowserDownloadEntry, BrowserDownloadEvent,
    BrowserDownloadsOperation, BrowserDownloadsParams, BrowserDownloadsResponse,
    BrowserEvaluateParams, BrowserEvaluateResponse, BrowserExposeBindingOperation,
    BrowserExposeBindingParams, BrowserExposeBindingResponse, BrowserFrameLocator,
    BrowserInitScriptOperation, BrowserInspectParams, BrowserInspectResponse,
    BrowserLayoutRelation, BrowserLocateEngine, BrowserLocateParams, BrowserLocateResponse,
    BrowserLocatedFrame, BrowserNavOperation, BrowserNavParams, BrowserNavResponse,
    BrowserNetworkWaitEntry, BrowserPdfParams, BrowserPdfResponse, BrowserScreenshotParams,
    BrowserScreenshotResponse, BrowserScreenshotScope, BrowserScrollIntoViewParams,
    BrowserScrollIntoViewResponse, BrowserSetContentParams, BrowserSetContentResponse,
    BrowserTabEntry, BrowserTabsParams, BrowserTabsResponse, BrowserWaitConditionKind,
    BrowserWaitForFunctionParams, BrowserWaitForFunctionResponse, BrowserWaitForLoadStateParams,
    BrowserWaitForLoadStateResponse, BrowserWaitForLoadStateState,
    BrowserWaitForNetworkResponseParams, BrowserWaitForNetworkResponseResponse,
    BrowserWaitForParams, BrowserWaitForRequestParams, BrowserWaitForRequestResponse,
    BrowserWaitForResponse, BrowserWaitForSelectorParams, BrowserWaitForSelectorResponse,
    BrowserWaitForSelectorState, BrowserWaitForState, BrowserWaitForUrlMatchKind,
    BrowserWaitForUrlParams, BrowserWaitForUrlResponse, BrowserWaitParams, BrowserWaitResponse,
    CaptureGifParams, CaptureScreenshotFormat, CaptureScreenshotParams, CaptureScreenshotResponse,
    CdpActivateTabParams, CdpActivateTabResponse, CdpActiveElementInfo, CdpBridgeHostReadback,
    CdpBridgeReloadAckReadback, CdpBridgeReloadParams, CdpBridgeReloadResponse, CdpCloseTabParams,
    CdpCloseTabResponse, CdpLargestContentfulPaintInfo, CdpNavigateAction, CdpNavigateTabParams,
    CdpNavigateTabResponse, CdpOpenTabParams, CdpOpenTabResponse, CdpPageTextInfo,
    CdpPageVitalsInfo, CdpTargetInfoParams, CdpTargetInfoResponse, CdpTargetOwner, ConsoleMessage,
    ElementInspection, ErrorData, FindParams, FindResponse, Health, HealthParams,
    HiddenDesktopPipFrameParams, HiddenDesktopPipFrameResponse, HiddenDesktopPipStreamStatus, Json,
    ObserveParams, Parameters, ReadTextParams, ScreenshotOperation, ScreenshotParams,
    ScreenshotResponse, SessionTarget, SetCaptureTargetParams, SetCaptureTargetResponse,
    SetPerceptionModeParams, SetPerceptionModeResponse, SetTargetParam, SetTargetParams,
    SynapseService, TargetResponse, TargetWire, WindowListEntry, WindowListParams,
    WindowListResponse, empty_input_schema, mcp_error, observe_include, observe_input,
    populate_audio_summary, populate_clipboard_summary, populate_detection_from_state,
    populate_fs_recent, read_text_request_uncached, resolve_read_text_request,
    set_capture_target_in_state, set_perception_mode_in_state, set_target_input_schema, tool,
    tool_router,
};
use crate::m1::{
    BrowserTabsActivationVisualReadback, BrowserTabsMutation, BrowserTabsOperation,
    CaptureRetryEvidence, ClipboardTimelineSample, FsTimelineEvent, effective_ocr_backend,
    hidden_desktop_input_from_worker_snapshot,
};
use crate::m3::activity_recorder::BrowserNavigationEvent;
use crate::server::session_continuity::PersistedCdpTargetOwner;
use crate::server::target_claims::{
    DEFAULT_TARGET_CLAIM_TTL_MS, TargetClaimAdoptParams, TargetClaimAdoptResponse,
    TargetClaimParams, TargetClaimResponse, TargetClaimStatusParams, TargetClaimStatusResponse,
    TargetClaimTargetParam, TargetReleaseParams, TargetReleaseResponse,
};
use crate::server::url_redaction::{
    redact_title_for_public_url_readback, redact_url_for_public_readback,
    redact_url_opt_for_public_readback,
};
use base64::Engine as _;
use rmcp::schemars::JsonSchema;
use rmcp::{RoleServer, model::ErrorCode, service::RequestContext};

use std::{
    collections::HashMap,
    io::Read as _,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

#[cfg(windows)]
use std::time::Instant;

#[cfg(windows)]
use chrono::{DateTime, Utc};
use image::{DynamicImage, ImageFormat, RgbaImage, codecs::jpeg::JpegEncoder};
#[cfg(windows)]
use image::{GrayImage, Luma};
#[cfg(windows)]
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use synapse_action::{BackendResolutionPolicy, ResolvedBackend, VigemBackend};
use synapse_core::{
    ForegroundContext, HudFieldError, HudReadings, InputBackendCapability, InputBackendDiagnostics,
    OcrResult, PERCEIVED_TEXT_UNTRUSTED_NOTICE, Profile, Rect, SuspectedInjectionAnnotation,
    error_codes, types::TimelineActor,
};
use synapse_perception::ObservationAssembler;
#[cfg(windows)]
use synapse_storage::{cf, decode_json, encode_json};

#[cfg(windows)]
use synapse_core::{HudExtractor, HudFieldSpec, HudReading, OcrBackend, Point, SCHEMA_VERSION};
#[cfg(windows)]
use synapse_perception::{
    FieldExtractionRequest, HudTemplate, OcrProvider, PerceptionError, PerceptionResult,
    SystemOcrProvider, TextRegion, extract_field, parse_hud_text, resolve_hud_region_rect,
};
#[cfg(windows)]
use synapse_reflex::ReflexRuntime;

const SCREENSHOT_SOURCE_OF_TRUTH: &str =
    "screenshot/GIF artifact bytes plus filesystem metadata readback";
const TARGET_FACADE_SOURCE_OF_TRUTH: &str =
    "MCP session target registry + CF_SESSIONS target rows + target claim registry";
const BROWSER_NAV_SOURCE_OF_TRUTH: &str =
    "Chrome bridge/CDP navigation command + session target ownership + CF_ACTION_LOG";
const BROWSER_NAV_READBACK_SOURCE_OF_TRUTH: &str = "page URL/title/readyState readback from chrome.tabs.get or Runtime.evaluate + daemon-tool-events.jsonl";
const BROWSER_WAIT_FACADE_SOURCE_OF_TRUTH: &str =
    "target-scoped browser wait predicate readback from DOM/URL/load/network/function state";
const BROWSER_WAIT_FACADE_READBACK_SOURCE_OF_TRUTH: &str =
    "browser_wait_for condition response plus daemon-tool-events.jsonl";
const BROWSER_SCREENSHOT_BRIDGE_RECONNECT_RETRY_WAIT_MS: u64 = 3_000;

#[derive(Clone, Debug)]
struct BrowserTabsWindowResolution {
    context: ForegroundContext,
    used_human_os_foreground_window: bool,
    discovery_source: &'static str,
    chromium_window_candidate_count: u32,
}

#[cfg(windows)]
#[derive(Clone, Debug)]
struct BrowserTabsActivationVisualProbe {
    window_title: String,
    bitmap_sha256: String,
    width: u32,
    height: u32,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum TargetOperation {
    #[default]
    Get,
    List,
    Set,
    Clear,
    Claim,
    Status,
    Adopt,
    Release,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum BrowserWaitOperation {
    ForCondition,
}

impl BrowserWaitOperation {
    const fn as_str(self) -> &'static str {
        match self {
            Self::ForCondition => "for_condition",
        }
    }
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct BrowserWaitFacadeParams {
    /// Wait operation to run. Supply exactly the matching `wait` spec object.
    operation: BrowserWaitOperation,
    /// `operation=for_condition`: unified wait condition spec.
    #[serde(default)]
    wait: Option<BrowserWaitParams>,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct BrowserWaitFacadeResponse {
    operation: BrowserWaitOperation,
    source_of_truth: String,
    readback_source_of_truth: String,
    wait: BrowserWaitResponse,
}

#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct TargetParams {
    #[serde(default)]
    operation: TargetOperation,
    #[serde(default)]
    target: Option<SetTargetParam>,
    #[serde(default)]
    owner_session_id: Option<String>,
    #[serde(default)]
    ttl_ms: Option<u64>,
    #[serde(default)]
    title_contains: Option<String>,
    #[serde(default)]
    process_name_contains: Option<String>,
    #[serde(default)]
    exclude_minimized: bool,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct TargetFacadeResponse {
    operation: TargetOperation,
    source_of_truth: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    target_state: Option<TargetResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    windows: Option<WindowListResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    claim: Option<TargetClaimResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    claim_status: Option<TargetClaimStatusResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    adopted: Option<TargetClaimAdoptResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    released: Option<TargetReleaseResponse>,
}

#[tool_router(router = m1_tool_router, vis = "pub(super)")]
impl SynapseService {
    #[tool(
        description = "Return server health. `detail` controls verbosity: \"compact\" (the default) returns each subsystem's structured status/verdict fields but omits the verbose human-readable `detail` prose to minimize token cost; \"full\" returns the complete legacy diagnostic detail for every subsystem. The chrome_bridge subsystem exposes its verdict as a structured `chrome_bridge` object in both modes."
    )]
    pub async fn health(
        &self,
        params: Parameters<HealthParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<Health>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "health",
            "tool.invocation kind=health"
        );
        let session_id = super::context::mcp_session_id_from_request_context(&request_context)?;
        Ok(Json(self.health_payload_for_session(
            session_id.as_deref(),
            params.0.detail,
        )))
    }

    #[tool(
        description = "Returns structured state of the session's active target window (set via set_target) or the foreground window when no target is set, plus surrounding context. include:[\"interactable\"] returns only interactable/form elements (edits, buttons, links, form widgets) without the structural depth cut — the lean shape for form-filling. Diagnostic blocks (input_backends, cdp probe evidence, capture config/runtime) are emitted only when include requests diagnostics (or include is omitted). If perceived text matches local prompt-injection heuristics, the response includes perceived_text_notice and suspected_injection annotations with source_path, span, score, heuristics, and evidence; clean responses omit them."
    )]
    pub async fn observe(
        &self,
        params: Parameters<ObserveParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<synapse_core::Observation>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "observe",
            "tool.invocation kind=observe"
        );
        let include = observe_include(&params.0);
        let session_id = super::context::mcp_session_id_from_request_context(&request_context)?;
        let target = self.request_session_target(&request_context)?;
        self.observe_with_target(params, include, target, session_id.as_deref())
            .await
    }

    #[cfg(test)]
    pub(crate) async fn observe_without_request_context_for_test(
        &self,
        params: Parameters<ObserveParams>,
    ) -> Result<Json<synapse_core::Observation>, ErrorData> {
        let include = observe_include(&params.0);
        self.observe_with_target(params, include, None, None).await
    }

    #[cfg(test)]
    pub(crate) async fn observe_for_mcp_session_id_for_test(
        &self,
        params: Parameters<ObserveParams>,
        mcp_session_id: &str,
    ) -> Result<Json<synapse_core::Observation>, ErrorData> {
        let include = observe_include(&params.0);
        self.observe_with_target(params, include, None, Some(mcp_session_id))
            .await
    }

    async fn observe_with_target(
        &self,
        params: Parameters<ObserveParams>,
        include: synapse_perception::ObserveInclude,
        target: Option<SessionTarget>,
        mcp_session_id: Option<&str>,
    ) -> Result<Json<synapse_core::Observation>, ErrorData> {
        let explicit_hwnd = params.0.window_hwnd;
        // A subtree_root drill-down is element/window perception even when the
        // include set looks global, so it always takes the window path.
        let needs_window = include.requires_window_perception() || params.0.subtree_root.is_some();
        // Global-only observes (fs/clipboard/audio) read host-wide state and
        // never need a window; do not resolve or refuse a target window for
        // them, so a filesystem-only readback succeeds even when the session's
        // active target is a Chrome CDP tab the window path refuses to
        // downgrade (#1508).
        let target_hwnd = if needs_window {
            perception_window_hwnd("observe", &target, explicit_hwnd)?
        } else {
            None
        };
        let cdp_target_id_hint = if !needs_window || explicit_hwnd.is_some() {
            None
        } else {
            target_cdp_id(&target)
        };
        let mut fs_timeline_events = Vec::new();
        // Scope the (non-Send) state guard so it is released before any await.
        let mut input = {
            let state = self.m1_state()?;
            let mut input = if needs_window {
                match observe_input(&state, &params.0, target_hwnd) {
                    Ok(input) => input,
                    Err(error) if params.0.subtree_root.is_none() => {
                        let Some(hwnd) = target_hwnd else {
                            return Err(error);
                        };
                        let Some(session_id) = mcp_session_id else {
                            return Err(error);
                        };
                        self.hidden_desktop_observe_input(
                            session_id,
                            hwnd,
                            crate::m1::observe_gather_depth(&params.0),
                            state.perception_mode,
                            error,
                        )?
                    }
                    Err(error) => return Err(error),
                }
            } else {
                crate::m1::global_only_input(&state)
            };
            if include.fs && input.fs_recent.is_empty() {
                fs_timeline_events = populate_fs_recent(&mut input, &state.fs_recent_tracker);
            }
            input
        };
        if let Some(since) = params.0.since_event_seq {
            input.recent_events.retain(|event| event.seq > since);
        }

        if include.elements {
            // #882: interactable mode filters semantically AFTER the gather, so
            // the web gather cap must cover the whole page (plus any requested
            // offset), not just one result page — otherwise the filter would
            // only ever see the first page of mostly-structural nodes.
            let cdp_max_nodes = if include.interactable_only {
                include
                    .element_offset
                    .saturating_add(include.max_subtree_nodes)
                    .max(crate::m1::find_cdp_max_nodes())
            } else {
                include.max_subtree_nodes
            };
            super::enrich_input_with_cdp_for_target(
                &mut input,
                include.max_subtree_depth,
                cdp_max_nodes,
                cdp_target_id_hint.as_deref(),
            )
            .await;
            super::enrich_input_with_browser_ocr(&mut input, include.max_subtree_nodes);
        }

        if include.audio && input.audio == synapse_core::AudioContext::default() {
            populate_audio_summary(&self.m3_state, &mut input);
        }
        if include.diagnostics {
            self.populate_input_backend_diagnostics(&mut input);
        }
        let clipboard_timeline_sample = if include.clipboard && input.clipboard_summary.is_none() {
            populate_clipboard_summary(&mut input)
        } else {
            None
        };
        self.resolve_input_profile_and_hud(&mut input, include.hud);
        {
            let mut state = self.m1_state()?;
            populate_detection_from_state(&mut state, &mut input);
        }
        let mut observation = ObservationAssembler::new()
            .assemble(include, input)
            .map_err(|err| mcp_error(err.code(), err.to_string()))?;
        attach_observation_hygiene_annotations(&mut observation)?;

        let mut state = self.m1_state()?;
        state.last_observed_foreground = Some(observation.foreground.clone());
        drop(state);
        self.persist_observation_for_mcp_session(&observation, "observe", mcp_session_id)?;
        self.record_timeline_enrichments(
            &observation,
            clipboard_timeline_sample.as_ref(),
            &fs_timeline_events,
        )?;
        Ok(Json(observation))
    }

    fn record_timeline_enrichments(
        &self,
        observation: &synapse_core::Observation,
        clipboard: Option<&ClipboardTimelineSample>,
        fs_events: &[FsTimelineEvent],
    ) -> Result<(), ErrorData> {
        if clipboard.is_none() && fs_events.is_empty() {
            return Ok(());
        }
        let recorder = self
            .m3_state
            .lock()
            .map_err(|_error| {
                mcp_error(
                    error_codes::TOOL_INTERNAL_ERROR,
                    "M3 service state lock poisoned while recording timeline enrichment",
                )
            })?
            .activity_recorder
            .clone();
        if let Some(recorder) = recorder {
            recorder.record_observation_enrichment(observation, clipboard, fs_events);
        }
        Ok(())
    }

    fn record_browser_navigation_timeline(&self, event: BrowserNavigationEvent) {
        let recorder = match self.m3_state.lock() {
            Ok(state) => state.activity_recorder.clone(),
            Err(_error) => {
                tracing::error!(
                    code = "TIMELINE_BROWSER_NAV_M3_LOCK_POISONED",
                    "M3 service state lock poisoned while recording MCP browser navigation"
                );
                return;
            }
        };
        if let Some(recorder) = recorder {
            let _ = recorder.record_browser_navigation(redact_browser_navigation_event(event));
        } else {
            tracing::error!(
                code = "TIMELINE_BROWSER_NAV_RECORDER_MISSING",
                "MCP browser navigation completed before the activity recorder was available"
            );
        }
    }

    #[tool(
        description = "Search visible accessibility nodes and detected entities. If matched result text contains suspected prompt injection, the response includes perceived_text_notice and suspected_injection annotations; clean responses omit them."
    )]
    pub async fn find(
        &self,
        params: Parameters<FindParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<FindResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "find",
            "tool.invocation kind=find"
        );
        let target = self.request_session_target(&request_context)?;
        let session_id = super::context::mcp_session_id_from_request_context(&request_context)?;
        self.find_with_target(params, target, session_id.as_deref())
            .await
    }

    #[cfg(test)]
    pub(crate) async fn find_without_request_context_for_test(
        &self,
        params: Parameters<FindParams>,
    ) -> Result<Json<FindResponse>, ErrorData> {
        self.find_with_target(params, None, None).await
    }

    async fn find_with_target(
        &self,
        params: Parameters<FindParams>,
        target: Option<SessionTarget>,
        mcp_session_id: Option<&str>,
    ) -> Result<Json<FindResponse>, ErrorData> {
        let explicit_hwnd = params.0.window_hwnd;
        let target_hwnd = perception_window_hwnd("find", &target, explicit_hwnd)?;
        let cdp_target_id_hint = if explicit_hwnd.is_some() {
            None
        } else {
            target_cdp_id(&target)
        };
        let mut input = {
            let mut state = self.m1_state()?;
            match super::build_find_input(&mut state, &params.0, target_hwnd) {
                Ok(input) => input,
                Err(error) => {
                    let Some(hwnd) = target_hwnd else {
                        return Err(error);
                    };
                    let Some(session_id) = mcp_session_id else {
                        return Err(error);
                    };
                    let mut input = self.hidden_desktop_find_input(
                        session_id,
                        hwnd,
                        state.perception_mode,
                        error,
                    )?;
                    populate_detection_from_state(&mut state, &mut input);
                    input
                }
            }
        };
        super::enrich_input_with_cdp_for_target(
            &mut input,
            super::find_snapshot_depth(),
            super::find_cdp_max_nodes(),
            cdp_target_id_hint.as_deref(),
        )
        .await;
        super::enrich_input_with_browser_ocr(&mut input, super::find_cdp_max_nodes());
        let mut response = super::match_find_input(&input, &params.0);
        attach_find_hygiene_annotations(&mut response);
        Ok(Json(response))
    }

    #[tool(
        description = "OCR text from a screen region, visible element, or target window. With window_hwnd or this MCP session's active window target, region is window-client-relative and OCR runs over passive target-window WGC BGRA capture; omitting region/element_id OCRs the whole target window using the WGC frame's native size. With no target it uses legacy screen-region/focused-element OCR. PrintWindow is disabled for normal targets because it executes target-process WM_PRINT/WM_PRINTCLIENT handlers, but session-owned hidden-desktop targets use an explicit per-desktop worker PrintWindow path. A clean OCR pass over a valid region that finds no glyphs is a valid empty observation, returned as success with no_text:true and empty full_text/words (not an OCR_NO_TEXT error); pass require_text:true to keep fail-closed absence. Backend/capture failures stay typed errors. If OCR text matches local prompt-injection heuristics, the response includes perceived_text_notice and suspected_injection annotations; clean responses omit them."
    )]
    pub async fn read_text(
        &self,
        params: Parameters<ReadTextParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<synapse_core::OcrResult>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "read_text",
            "tool.invocation kind=read_text"
        );
        // #703: a web element id (cdcd sentinel) is not a UIA element, so the
        // element-bounds path cannot resolve it. OCR it from a CDP
        // element-clipped screenshot instead of failing with a stale-UIA error.
        #[cfg(windows)]
        if params.0.region.is_none()
            && let Some(element_id) = params.0.element_id.as_ref()
            && let Some(backend_node_id) = synapse_a11y::cdp_backend_from_element_id(element_id)
        {
            let mut result = self
                .read_text_web_element(element_id, backend_node_id, &params.0)
                .await?;
            crate::m1::enforce_require_text(&result, params.0.require_text)?;
            attach_ocr_hygiene_annotations(&mut result);
            return Ok(Json(result));
        }
        let session_id = super::context::mcp_session_id_from_request_context(&request_context)?;
        let target = self.request_session_target(&request_context)?;
        let target_hwnd = perception_window_hwnd("read_text", &target, params.0.window_hwnd)?;
        self.read_text_with_target_hwnd(params, target_hwnd, session_id.as_deref())
    }

    #[cfg(test)]
    pub(crate) fn read_text_without_request_context_for_test(
        &self,
        params: Parameters<ReadTextParams>,
    ) -> Result<Json<synapse_core::OcrResult>, ErrorData> {
        self.read_text_with_target_hwnd(params, None, None)
    }

    fn read_text_with_target_hwnd(
        &self,
        params: Parameters<ReadTextParams>,
        target_hwnd: Option<i64>,
        mcp_session_id: Option<&str>,
    ) -> Result<Json<synapse_core::OcrResult>, ErrorData> {
        let normal_result = ({
            let state = self.m1_state()?;
            resolve_read_text_request(&state, &params.0, target_hwnd)
        })
        .and_then(|request| self.read_text_request_with_cache(request));
        match normal_result {
            Ok(mut result) => {
                // #1557 fail-closed gate, applied to the final (post-cache)
                // result so require_text is honoured on a CF_OCR_CACHE hit too.
                crate::m1::enforce_require_text(&result, params.0.require_text)?;
                attach_ocr_hygiene_annotations(&mut result);
                Ok(Json(result))
            }
            Err(error) => {
                let Some(hwnd) = params.0.window_hwnd.or(target_hwnd) else {
                    return Err(error);
                };
                let Some(session_id) = mcp_session_id else {
                    return Err(error);
                };
                let mut result =
                    self.read_text_hidden_desktop(&params.0, session_id, hwnd, error)?;
                crate::m1::enforce_require_text(&result, params.0.require_text)?;
                attach_ocr_hygiene_annotations(&mut result);
                Ok(Json(result))
            }
        }
    }

    #[tool(
        description = "Capture a PNG/JPEG screenshot. With an active session raw CDP target, captures that exact browser tab through Page.captureScreenshot. The normal authenticated Chrome bridge is debugger-free and refuses Page.captureScreenshot before queueing any Chrome command, because Chrome's debugger infobar changes viewport/layout and breaks coordinate truth; use raw CDP on a dedicated silent automation profile or passive window capture instead. With window_hwnd or a window target, captures that window in the background using passive per-window WGC and interprets region as client-relative. With no target, preserves legacy foreground-window or absolute screen-region capture. PrintWindow is disabled for normal targets because it executes target-process WM_PRINT/WM_PRINTCLIENT handlers, but session-owned hidden-desktop targets use an explicit per-desktop worker PrintWindow path. Optional max_pixels and/or max_long_edge downscale the written image aspect-preserving (Lanczos3) to fit a vision-model pixel budget (e.g. max_long_edge=1568 / max_pixels=1150000 for the Claude 4.6 family, 2576 / 3750000 for Opus 4.7; the more restrictive wins). They are no-ops when the native capture already fits. The response always reports native_width/native_height and the applied scale (written_long_edge/native_long_edge, 1.0 when not downscaled) so a coordinate read off the written image maps back to native pixels by multiplying by 1.0/scale. To inspect a small or dense UI region at full native resolution (the computer-use 'zoom' affordance), pass a tight client-relative region and omit the pixel budget."
    )]
    pub async fn capture_screenshot(
        &self,
        params: Parameters<CaptureScreenshotParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<CaptureScreenshotResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "capture_screenshot",
            "tool.invocation kind=capture_screenshot"
        );
        let session_id = super::context::mcp_session_id_from_request_context(&request_context)?;
        let target = self.request_session_target(&request_context)?;
        if params.0.window_hwnd.is_none()
            && let Some(SessionTarget::Cdp {
                window_hwnd,
                cdp_target_id,
            }) = target.as_ref()
        {
            let session_id = session_id.as_deref().ok_or_else(|| {
                mcp_error(
                    error_codes::HTTP_SESSION_INVALID,
                    "capture_screenshot requires an MCP session id for a CDP session target",
                )
            })?;
            return self
                .capture_cdp_target_screenshot_to_file(
                    &params.0,
                    session_id,
                    *window_hwnd,
                    cdp_target_id,
                )
                .await
                .map(Json);
        }
        let session_target_hwnd =
            perception_window_hwnd("capture_screenshot", &target, params.0.window_hwnd)?;
        if let Some(window_hwnd) = params.0.window_hwnd.or(session_target_hwnd) {
            if let Some(session_id) = session_id.as_deref() {
                if self
                    .hidden_desktop_context_for_session(session_id, window_hwnd)?
                    .is_some()
                {
                    let original_error = mcp_error(
                        error_codes::TARGET_WINDOW_NOT_FOUND,
                        format!(
                            "capture_screenshot hidden desktop target hwnd {window_hwnd:#x} was not found in session {session_id}"
                        ),
                    );
                    return self
                        .capture_hidden_desktop_screenshot_to_file(
                            &params.0,
                            Some(session_id),
                            window_hwnd,
                            original_error,
                        )
                        .map(Json);
                }
            }
            let normal_result = (|| {
                let target_context = resolve_capture_target_window_context(window_hwnd)?;
                let region = match params.0.region {
                    Some(client_region) => synapse_capture::client_region_to_window_region(
                        window_hwnd,
                        client_region,
                    )
                    .map_err(|error| {
                        mcp_error(
                            error.code(),
                            format!(
                                "capture_screenshot could not convert client-relative region {client_region:?} for target {window_hwnd:#x}: {error}"
                            ),
                        )
                    })?,
                    None => synapse_capture::window_capture_region(window_hwnd).map_err(|error| {
                        mcp_error(
                            error.code(),
                            format!(
                                "capture_screenshot could not resolve target bitmap bounds for {window_hwnd:#x}: {error}"
                            ),
                        )
                    })?,
                };
                capture_target_window_screenshot_to_file(
                    &params.0,
                    window_hwnd,
                    region,
                    Some(target_context),
                )
            })();
            return match normal_result {
                Ok(response) => Ok(Json(response)),
                Err(error) => self
                    .capture_hidden_desktop_screenshot_to_file(
                        &params.0,
                        session_id.as_deref(),
                        window_hwnd,
                        error,
                    )
                    .map(Json),
            };
        }

        let foreground = if params.0.region.is_some() {
            synapse_a11y::current_foreground_context().ok()
        } else {
            Some(synapse_a11y::current_foreground_context().map_err(|error| {
                mcp_error(
                    error.code(),
                    format!("capture_screenshot could not resolve the foreground window: {error}"),
                )
            })?)
        };
        let region = params
            .0
            .region
            .or_else(|| foreground.as_ref().map(|context| context.window_bounds))
            .ok_or_else(|| {
                mcp_error(
                    error_codes::CAPTURE_TARGET_INVALID,
                    "capture_screenshot requires a region when no foreground window is available",
                )
            })?;
        capture_screen_screenshot_to_file(&params.0, region, foreground).map(Json)
    }

    #[tool(
        description = "Public perception artifact facade. operation=capture writes a PNG/JPEG screenshot through the same verified artifact path as capture_screenshot. operation=gif records a bounded target-window GIF through the same artifact path as capture_gif. Both operations require an absolute output path, reject operation-irrelevant parameters, and return the physical artifact readback (path, bytes, dimensions, backend, and hash for still images)."
    )]
    pub async fn screenshot(
        &self,
        params: Parameters<ScreenshotParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<ScreenshotResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "screenshot",
            operation = screenshot_operation_name(params.0.operation),
            "tool.invocation kind=screenshot"
        );
        let params = params.0;
        match params.operation {
            ScreenshotOperation::Capture => {
                validate_screenshot_capture_facade_params(&params)?;
                let capture = self
                    .capture_screenshot(
                        Parameters(CaptureScreenshotParams {
                            path: params.path,
                            region: params.region,
                            window_hwnd: params.window_hwnd,
                            overwrite: params.overwrite,
                            max_pixels: params.max_pixels,
                            max_long_edge: params.max_long_edge,
                        }),
                        request_context,
                    )
                    .await?
                    .0;
                Ok(Json(ScreenshotResponse {
                    operation: ScreenshotOperation::Capture,
                    source_of_truth: SCREENSHOT_SOURCE_OF_TRUTH.to_owned(),
                    capture: Some(capture),
                    gif: None,
                }))
            }
            ScreenshotOperation::Gif => {
                validate_screenshot_gif_facade_params(&params)?;
                let gif = self
                    .capture_gif(
                        Parameters(CaptureGifParams {
                            path: params.path,
                            duration_ms: params.duration_ms,
                            interval_ms: params.interval_ms,
                            window_hwnd: params.window_hwnd,
                            max_long_edge: params.max_long_edge,
                            overwrite: params.overwrite,
                        }),
                        request_context,
                    )
                    .await?
                    .0;
                Ok(Json(ScreenshotResponse {
                    operation: ScreenshotOperation::Gif,
                    source_of_truth: SCREENSHOT_SOURCE_OF_TRUTH.to_owned(),
                    capture: None,
                    gif: Some(gif),
                }))
            }
        }
    }

    #[tool(
        description = "Capture a browser page screenshot from the calling session's owned normal Chrome tab through the popup-safe Chrome bridge, without Page.captureScreenshot or debugger screenshot attach. Supports scope=viewport, full_page, clip (page CSS x/y/w/h), and element (normal bridge element_id), PNG/JPEG format, JPEG quality, omit_background best-effort PNG transparency, and selector/element masks restored after capture. Uses chrome.scripting for page metrics/masks/scroll plus chrome.tabs.captureVisibleTab tile stitching, temporarily activates only the requested tab inside its existing Chrome window, and may focus that Chrome window on Windows because captureVisibleTab can fail image readback otherwise; the response reports required_foreground and restore readback. Optional max_pixels and/or max_long_edge downscale the written image aspect-preserving to fit a vision-model pixel budget (see capture_screenshot); the response reports native_width/native_height and the applied scale."
    )]
    pub async fn browser_screenshot(
        &self,
        params: Parameters<BrowserScreenshotParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<BrowserScreenshotResponse>, ErrorData> {
        const TOOL: &str = "browser_screenshot";
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = TOOL,
            "tool.invocation kind=browser_screenshot"
        );
        let session_id = require_target_session_id(&request_context)?;
        let validation = validate_browser_screenshot_params(&params.0)?;
        let resolution_target = params
            .0
            .cdp_target_id
            .as_deref()
            .or(validation.element_target.as_deref())
            .or(validation.mask_target.as_deref());
        let request_details = json!({
            "session_id": &session_id,
            "window_hwnd": params.0.window_hwnd,
            "requested_cdp_target": cdp_target_id_audit_ref(resolution_target),
            "path": &params.0.path,
            "scope": params.0.scope,
            "clip": params.0.clip,
            "element_id": params.0.element_id.as_deref(),
            "mask_count": params.0.masks.len(),
            "format": validation.format,
            "quality": params.0.quality,
            "omit_background": params.0.omit_background,
            "required_foreground": false,
            "phase": "target_resolution",
        });
        let resolution = self.resolve_cdp_tab_mutation_target(
            TOOL,
            &session_id,
            params.0.window_hwnd,
            resolution_target,
        );
        let (window_hwnd, cdp_target_id) = self.audit_cdp_target_resolution_result(
            TOOL,
            &session_id,
            &request_details,
            resolution,
        )?;
        validate_browser_screenshot_target_ids(&validation, &cdp_target_id)?;
        let request_details = json!({
            "session_id": &session_id,
            "window_hwnd": window_hwnd,
            "cdp_target_id": &cdp_target_id,
            "path": &params.0.path,
            "scope": params.0.scope,
            "clip": params.0.clip,
            "element_id": params.0.element_id.as_deref(),
            "mask_count": params.0.masks.len(),
            "format": validation.format,
            "quality": params.0.quality,
            "omit_background": params.0.omit_background,
            "required_foreground": false,
        });
        self.audit_action_started_with_details_for_session(TOOL, &request_details, &session_id)?;
        let result = self
            .browser_screenshot_impl(
                &params.0,
                &validation,
                &session_id,
                window_hwnd,
                &cdp_target_id,
            )
            .await;
        self.audit_action_result_for_session(TOOL, &result, &session_id)?;
        result.map(Json)
    }

    #[tool(
        description = "Render the calling session's owned normal Chrome tab to PDF through the already-open Chrome bridge using a narrow chrome.debugger Page.printToPDF lane. Supports paper size, margins, landscape, print background, CSS page size, header/footer templates, page ranges, and scale. Writes a PDF file, returns byte count/hash and target readback, and never launches another Chrome profile."
    )]
    pub async fn browser_pdf(
        &self,
        params: Parameters<BrowserPdfParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<BrowserPdfResponse>, ErrorData> {
        const TOOL: &str = "browser_pdf";
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = TOOL,
            "tool.invocation kind=browser_pdf"
        );
        let session_id = require_target_session_id(&request_context)?;
        let validation = validate_browser_pdf_params(&params.0)?;
        let request_details = json!({
            "session_id": &session_id,
            "window_hwnd": params.0.window_hwnd,
            "requested_cdp_target": cdp_target_id_audit_ref(params.0.cdp_target_id.as_deref()),
            "path": &params.0.path,
            "landscape": params.0.landscape,
            "print_background": params.0.print_background,
            "paper_width": params.0.paper_width,
            "paper_height": params.0.paper_height,
            "required_foreground": false,
            "phase": "target_resolution",
        });
        let resolution = self.resolve_cdp_tab_mutation_target(
            TOOL,
            &session_id,
            params.0.window_hwnd,
            params.0.cdp_target_id.as_deref(),
        );
        let (window_hwnd, cdp_target_id) = self.audit_cdp_target_resolution_result(
            TOOL,
            &session_id,
            &request_details,
            resolution,
        )?;
        let request_details = json!({
            "session_id": &session_id,
            "window_hwnd": window_hwnd,
            "cdp_target_id": &cdp_target_id,
            "path": &params.0.path,
            "landscape": params.0.landscape,
            "print_background": params.0.print_background,
            "paper_width": params.0.paper_width,
            "paper_height": params.0.paper_height,
            "required_foreground": false,
        });
        self.audit_action_started_with_details_for_session(TOOL, &request_details, &session_id)?;
        let result = self
            .browser_pdf_impl(&params.0, &validation, window_hwnd, &cdp_target_id)
            .await;
        self.audit_action_result_for_session(TOOL, &result, &session_id)?;
        result.map(Json)
    }

    #[tool(
        description = "List, wait for, save, or move downloads from the already-open normal Chrome profile through the bundled chrome.downloads bridge (#1106-#1109). operation=list is read-only; operation=wait blocks until a matching download reaches state=complete by default or the requested state; operation=save/move waits for a completed match then copies or moves it to an absolute path with byte count and SHA-256 readback. Never launches a second Chrome profile, never uses nativeMessaging, and never takes OS foreground."
    )]
    pub async fn browser_downloads(
        &self,
        params: Parameters<BrowserDownloadsParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<BrowserDownloadsResponse>, ErrorData> {
        const TOOL: &str = "browser_downloads";
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = TOOL,
            "tool.invocation kind=browser_downloads"
        );
        let session_id = require_target_session_id(&request_context)?;
        let validation = validate_browser_downloads_params(params.0)?;
        let window_resolution = self.resolve_browser_tabs_window_context(
            TOOL,
            &session_id,
            validation.params.window_hwnd,
            true,
        )?;
        let window_context = window_resolution.context;
        let used_human_os_foreground_window = window_resolution.used_human_os_foreground_window;
        let request_details = json!({
            "session_id": &session_id,
            "operation": validation.params.operation,
            "window_hwnd": window_context.hwnd,
            "window_title": &window_context.window_title,
            "process_name": &window_context.process_name,
            "used_human_os_foreground_window": used_human_os_foreground_window,
            "window_discovery_source": &window_resolution.discovery_source,
            "chromium_window_candidate_count": window_resolution.chromium_window_candidate_count,
            "download_id": validation.params.download_id,
            "url_contains": validation.params.url_contains.as_deref(),
            "filename_contains": validation.params.filename_contains.as_deref(),
            "mime_contains": validation.params.mime_contains.as_deref(),
            "state": validation.params.state.as_deref(),
            "path": validation.output_path.as_ref().map(|path| path.display().to_string()),
            "overwrite": validation.params.overwrite,
            "required_foreground": false,
            "no_debugger_attach": true,
        });
        self.audit_action_started_with_details_for_session(TOOL, &request_details, &session_id)?;
        let result = self
            .browser_downloads_impl(
                &session_id,
                window_context,
                used_human_os_foreground_window,
                &validation,
            )
            .await;
        self.audit_action_result_for_session(TOOL, &result, &session_id)?;
        result.map(Json)
    }

    #[tool(
        description = "Read-only Picture-in-Picture frame for a session-owned hidden desktop. Captures one hidden-desktop window frame through the per-desktop PrintWindow worker and writes it to the requested PNG/JPEG path as the viewer surface. It never forwards clicks, keys, or any operator input into the hidden desktop; repeat calls form the frame stream. If the watched session is already closed, returns stream_status=ended without writing a frame."
    )]
    pub async fn hidden_desktop_pip_frame(
        &self,
        params: Parameters<HiddenDesktopPipFrameParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<HiddenDesktopPipFrameResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "hidden_desktop_pip_frame",
            "tool.invocation kind=hidden_desktop_pip_frame"
        );
        let current_session_id = super::context::mcp_session_id_from_request_context(
            &request_context,
        )?
        .ok_or_else(|| {
            mcp_error(
                error_codes::HTTP_SESSION_INVALID,
                "hidden_desktop_pip_frame requires an MCP session id",
            )
        })?;
        let watched_session_id = params
            .0
            .watched_session_id
            .clone()
            .unwrap_or(current_session_id);
        super::session_tools::validate_session_id(&watched_session_id)?;
        self.hidden_desktop_pip_frame_to_file(&params.0, &watched_session_id)
            .map(Json)
    }

    #[tool(description = "Set the active capture target")]
    pub async fn set_capture_target(
        &self,
        params: Parameters<SetCaptureTargetParams>,
    ) -> Result<Json<SetCaptureTargetResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "set_capture_target",
            "tool.invocation kind=set_capture_target"
        );
        let mut state = self.m1_state()?;
        set_capture_target_in_state(&mut state, params.0).map(Json)
    }

    #[tool(description = "Set the active perception mode")]
    pub async fn set_perception_mode(
        &self,
        params: Parameters<SetPerceptionModeParams>,
    ) -> Result<Json<SetPerceptionModeResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "set_perception_mode",
            "tool.invocation kind=set_perception_mode"
        );
        let mut state = self.m1_state()?;
        set_perception_mode_in_state(&mut state, &params.0).map(Json)
    }

    #[tool(
        description = "Bind this MCP session's active perception target. Accepted target shapes are {\"kind\":\"window\",\"window_hwnd\":<integer>} or {\"kind\":\"cdp\",\"window_hwnd\":<integer>,\"cdp_target_id\":\"<target id>\"}. While set, observe/find/read_text/capture_screenshot perceive THIS window/tab without foregrounding it, so many agents observe different windows concurrently. Legacy {\"hwnd\":...} is intentionally unsupported. Validates the window is live and snapshottable, echoing its title/process. Errors TARGET_WINDOW_NOT_FOUND for a dead/invalid HWND.",
        input_schema = set_target_input_schema()
    )]
    pub async fn set_target(
        &self,
        params: Parameters<SetTargetParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<TargetResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "set_target",
            "tool.invocation kind=set_target"
        );
        let session_id = require_target_session_id(&request_context)?;
        let (target, wire, window_title, process_name) = match params.0.target {
            SetTargetParam::Window { window_hwnd } => {
                let (title, process) =
                    self.validate_target_window_for_session(&session_id, window_hwnd)?;
                (
                    SessionTarget::Window { hwnd: window_hwnd },
                    TargetWire::Window { window_hwnd },
                    Some(title),
                    Some(process),
                )
            }
            SetTargetParam::Cdp {
                window_hwnd,
                cdp_target_id,
            } => {
                validate_cdp_target_id(&cdp_target_id)?;
                let (title, process) =
                    self.validate_target_window_for_session(&session_id, window_hwnd)?;
                self.ensure_cdp_target_bindable_for_window(
                    &session_id,
                    window_hwnd,
                    &cdp_target_id,
                )
                .await?;
                (
                    SessionTarget::Cdp {
                        window_hwnd,
                        cdp_target_id: cdp_target_id.clone(),
                    },
                    TargetWire::Cdp {
                        window_hwnd,
                        cdp_target_id,
                    },
                    Some(title),
                    Some(process),
                )
            }
        };
        let previous = self.set_session_target(&session_id, target)?;
        tracing::info!(
            code = "SESSION_TARGET_SET",
            session_id = %session_id,
            window_title = window_title.as_deref().unwrap_or_default(),
            process_name = process_name.as_deref().unwrap_or_default(),
            "readback=session_target outcome=set"
        );
        Ok(Json(TargetResponse {
            session_id,
            previous,
            current: Some(wire),
            window_title,
            process_name,
        }))
    }

    #[tool(
        description = "Return this MCP session's active perception target, or null when none is set.",
        input_schema = empty_input_schema()
    )]
    pub async fn get_target(
        &self,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<TargetResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "get_target",
            "tool.invocation kind=get_target"
        );
        let session_id = require_target_session_id(&request_context)?;
        let current = self.get_session_target_wire(&session_id)?;
        Ok(Json(TargetResponse {
            session_id,
            previous: None,
            current,
            window_title: None,
            process_name: None,
        }))
    }

    #[tool(
        description = "Clear this MCP session's active perception target, reverting observe/find/read_text to the global foreground.",
        input_schema = empty_input_schema()
    )]
    pub async fn clear_target(
        &self,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<TargetResponse>, ErrorData> {
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "clear_target",
            "tool.invocation kind=clear_target"
        );
        let session_id = require_target_session_id(&request_context)?;
        let previous = self.clear_session_target(&session_id)?;
        tracing::info!(
            code = "SESSION_TARGET_CLEARED",
            session_id = %session_id,
            had_target = previous.is_some(),
            "readback=session_target outcome=cleared"
        );
        Ok(Json(TargetResponse {
            session_id,
            previous,
            current: None,
            window_title: None,
            process_name: None,
        }))
    }

    #[tool(
        description = "Public target facade. operation=get reads this MCP session's active target; operation=list passively lists live top-level windows; operation=set/clear update this session target; operation=claim/status/adopt/release wraps advisory target ownership leases. All operations use strict enum routing and return/name the physical source of truth."
    )]
    pub async fn target(
        &self,
        params: Parameters<TargetParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<TargetFacadeResponse>, ErrorData> {
        let params = params.0;
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "target",
            operation = target_operation_name(params.operation),
            "tool.invocation kind=target"
        );

        let response = match params.operation {
            TargetOperation::Get => {
                validate_target_get_params(&params)?;
                let target_state = self.get_target(request_context).await?.0;
                TargetFacadeResponse::target_state(TargetOperation::Get, target_state)
            }
            TargetOperation::List => {
                validate_target_list_params(&params)?;
                let windows = self
                    .window_list(
                        Parameters(WindowListParams {
                            title_contains: params.title_contains,
                            process_name_contains: params.process_name_contains,
                            exclude_minimized: params.exclude_minimized,
                        }),
                        request_context,
                    )
                    .await?
                    .0;
                TargetFacadeResponse::windows(windows)
            }
            TargetOperation::Set => {
                validate_target_set_params(&params)?;
                let target = params.target.ok_or_else(|| {
                    target_facade_error(
                        TargetOperation::Set,
                        "target operation=set requires target",
                        "pass a target from target operation=list or browser_tabs operation=list",
                        "target",
                    )
                })?;
                let target_state = self
                    .set_target(Parameters(SetTargetParams { target }), request_context)
                    .await?
                    .0;
                TargetFacadeResponse::target_state(TargetOperation::Set, target_state)
            }
            TargetOperation::Clear => {
                validate_target_clear_params(&params)?;
                let target_state = self.clear_target(request_context).await?.0;
                TargetFacadeResponse::target_state(TargetOperation::Clear, target_state)
            }
            TargetOperation::Claim => {
                validate_target_claim_params(&params)?;
                let claim = self
                    .target_claim(
                        Parameters(TargetClaimParams {
                            target: params.target.map(target_claim_param_from_set),
                            ttl_ms: params.ttl_ms.unwrap_or(DEFAULT_TARGET_CLAIM_TTL_MS),
                        }),
                        request_context,
                    )
                    .await?
                    .0;
                TargetFacadeResponse::claim(claim)
            }
            TargetOperation::Status => {
                validate_target_status_params(&params)?;
                let claim_status = self
                    .target_claim_status(
                        Parameters(TargetClaimStatusParams {
                            target: params.target.map(target_claim_param_from_set),
                        }),
                        request_context,
                    )
                    .await?
                    .0;
                TargetFacadeResponse::claim_status(claim_status)
            }
            TargetOperation::Adopt => {
                validate_target_adopt_params(&params)?;
                let owner_session_id = params.owner_session_id.ok_or_else(|| {
                    target_facade_error(
                        TargetOperation::Adopt,
                        "target operation=adopt requires owner_session_id",
                        "read target operation=status or session operation=list, then pass the owner_session_id to adopt",
                        "owner_session_id",
                    )
                })?;
                let adopted = self
                    .target_claim_adopt(
                        Parameters(TargetClaimAdoptParams {
                            owner_session_id,
                            target: params.target.map(target_claim_param_from_set),
                            ttl_ms: params.ttl_ms.unwrap_or(DEFAULT_TARGET_CLAIM_TTL_MS),
                        }),
                        request_context,
                    )
                    .await?
                    .0;
                TargetFacadeResponse::adopted(adopted)
            }
            TargetOperation::Release => {
                validate_target_release_params(&params)?;
                let released = self
                    .target_release(
                        Parameters(TargetReleaseParams {
                            target: params.target.map(target_claim_param_from_set),
                        }),
                        request_context,
                    )
                    .await?
                    .0;
                TargetFacadeResponse::released(released)
            }
        };
        Ok(Json(response))
    }

    #[tool(
        description = "Enumerate visible top-level windows as a passive snapshot — no activation, no foregrounding, no debugger attach (same non-interference contract as observe). Each entry has hwnd, pid, process_name, process_path, window_title, bounds, monitor, minimized/foreground/fullscreen flags, a Chromium hint, and any target-claim owner. The `target` field round-trips directly into set_target so you can bind a background window without shelling out or foregrounding. `is_foreground` / `human_os_foreground_hwnd` mark the human's window so agents can avoid it. To enumerate Chrome tabs, bind the browser window then use the Chrome bridge / cdp_target_info. Filterable by title_contains / process_name_contains; minimized windows are included by default (they are valid background targets)."
    )]
    pub async fn window_list(
        &self,
        params: Parameters<WindowListParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<WindowListResponse>, ErrorData> {
        const TOOL: &str = "window_list";
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = TOOL,
            "tool.invocation kind=window_list"
        );
        let session_id = require_target_session_id(&request_context)?;
        self.window_list_impl(&session_id, params.0).map(Json)
    }

    fn window_list_impl(
        &self,
        session_id: &str,
        params: WindowListParams,
    ) -> Result<WindowListResponse, ErrorData> {
        let now = super::session_registry::unix_time_ms_now();
        let contexts = synapse_a11y::visible_top_level_window_contexts().map_err(|error| {
            mcp_error(
                error.code(),
                format!("window_list could not enumerate top-level windows: {error}"),
            )
        })?;
        let human_os_foreground_hwnd = synapse_a11y::current_foreground_context()
            .ok()
            .map(|c| c.hwnd);

        // Annotate each window with its owning session from the durable
        // target-claim registry. A CDP claim still pins a window_hwnd, so both
        // claim shapes collapse onto a window row.
        let claims_by_owner = self.target_claim_reads_by_owner()?;
        let mut claim_by_hwnd: HashMap<
            i64,
            (String, crate::server::target_claims::TargetClaimRead),
        > = HashMap::new();
        for (owner, claims) in &claims_by_owner {
            for claim in claims {
                let hwnd = match &claim.target {
                    TargetWire::Window { window_hwnd } => *window_hwnd,
                    TargetWire::Cdp { window_hwnd, .. } => *window_hwnd,
                };
                claim_by_hwnd
                    .entry(hwnd)
                    .or_insert_with(|| (owner.clone(), claim.clone()));
            }
        }

        let title_filter = params.title_contains.as_deref().map(str::to_lowercase);
        let process_filter = params
            .process_name_contains
            .as_deref()
            .map(str::to_lowercase);

        let mut windows = Vec::with_capacity(contexts.len());
        for context in contexts {
            if let Some(filter) = title_filter.as_deref() {
                if !context.window_title.to_lowercase().contains(filter) {
                    continue;
                }
            }
            if let Some(filter) = process_filter.as_deref() {
                if !context.process_name.to_lowercase().contains(filter) {
                    continue;
                }
            }
            let is_minimized = synapse_a11y::is_window_minimized(context.hwnd).unwrap_or(false);
            if params.exclude_minimized && is_minimized {
                continue;
            }
            let (claimed_by_session_id, target_claim) = match claim_by_hwnd.get(&context.hwnd) {
                Some((owner, claim)) => (Some(owner.clone()), Some(claim.clone())),
                None => (None, None),
            };
            windows.push(WindowListEntry {
                hwnd: context.hwnd,
                pid: context.pid,
                process_name: context.process_name.clone(),
                process_path: context.process_path.clone(),
                window_title: context.window_title.clone(),
                window_bounds: context.window_bounds,
                monitor_index: context.monitor_index,
                dpi_scale: context.dpi_scale,
                is_minimized,
                is_foreground: human_os_foreground_hwnd == Some(context.hwnd),
                is_fullscreen: context.is_fullscreen,
                is_dwm_composed: context.is_dwm_composed,
                is_chromium: synapse_a11y::is_chromium_family(&context.process_name),
                claimed_by_session_id,
                target_claim,
                target: TargetWire::Window {
                    window_hwnd: context.hwnd,
                },
            });
        }

        tracing::info!(
            code = "MCP_WINDOW_LIST_READBACK",
            session_id = %session_id,
            window_count = windows.len(),
            human_os_foreground_hwnd = human_os_foreground_hwnd.unwrap_or(0),
            "readback=window_list passive snapshot (no activation, no attach)"
        );

        Ok(WindowListResponse {
            session_id: session_id.to_owned(),
            now_unix_ms: now,
            human_os_foreground_hwnd,
            window_count: windows.len(),
            windows,
            source_of_truth:
                "synapse_a11y::visible_top_level_window_contexts (EnumWindows + visibility filter) + CF target-claim registry"
                    .to_owned(),
        })
    }

    fn validate_target_window_for_session(
        &self,
        session_id: &str,
        hwnd: i64,
    ) -> Result<(String, String), ErrorData> {
        match validate_target_window(hwnd) {
            Ok(target) => Ok(target),
            Err(error) => {
                let Some(context) = self.hidden_desktop_context_for_session(session_id, hwnd)?
                else {
                    return Err(error);
                };
                Ok((context.window_title, context.process_name))
            }
        }
    }

    fn hidden_desktop_observe_input(
        &self,
        session_id: &str,
        hwnd: i64,
        depth: u32,
        mode: synapse_core::PerceptionMode,
        original_error: ErrorData,
    ) -> Result<synapse_perception::ObservationInput, ErrorData> {
        let Some(snapshot) =
            self.hidden_desktop_snapshot_for_session(session_id, hwnd, depth, original_error)?
        else {
            return Err(mcp_error(
                error_codes::TARGET_WINDOW_NOT_FOUND,
                format!(
                    "hidden desktop target hwnd {hwnd:#x} was not found in session {session_id}"
                ),
            ));
        };
        Ok(hidden_desktop_input_from_worker_snapshot(
            snapshot.tree,
            snapshot.context,
            mode,
        ))
    }

    fn hidden_desktop_find_input(
        &self,
        session_id: &str,
        hwnd: i64,
        mode: synapse_core::PerceptionMode,
        original_error: ErrorData,
    ) -> Result<synapse_perception::ObservationInput, ErrorData> {
        let Some(snapshot) = self.hidden_desktop_snapshot_for_session(
            session_id,
            hwnd,
            super::find_snapshot_depth(),
            original_error,
        )?
        else {
            return Err(mcp_error(
                error_codes::TARGET_WINDOW_NOT_FOUND,
                format!(
                    "hidden desktop target hwnd {hwnd:#x} was not found in session {session_id}"
                ),
            ));
        };
        Ok(hidden_desktop_input_from_worker_snapshot(
            snapshot.tree,
            snapshot.context,
            mode,
        ))
    }

    fn hidden_desktop_context_for_session(
        &self,
        session_id: &str,
        hwnd: i64,
    ) -> Result<Option<ForegroundContext>, ErrorData> {
        let Some(hidden_desktop) = self.session_hidden_desktop_readback(session_id)? else {
            return Ok(None);
        };
        for desktop_name in &hidden_desktop.desktop_names {
            match crate::desktop_worker::hidden_desktop_window_context(desktop_name, hwnd) {
                Ok(context) => return Ok(Some(context)),
                Err(error) if hidden_worker_target_miss(&error) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(None)
    }

    fn hidden_desktop_snapshot_for_session(
        &self,
        session_id: &str,
        hwnd: i64,
        depth: u32,
        original_error: ErrorData,
    ) -> Result<Option<crate::desktop_worker::HiddenDesktopSnapshot>, ErrorData> {
        let Some(hidden_desktop) = self.session_hidden_desktop_readback(session_id)? else {
            return Err(original_error);
        };
        for desktop_name in &hidden_desktop.desktop_names {
            match crate::desktop_worker::hidden_desktop_window_snapshot(desktop_name, hwnd, depth) {
                Ok(snapshot) => return Ok(Some(snapshot)),
                Err(error) if hidden_worker_target_miss(&error) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(None)
    }

    fn hidden_desktop_capture_for_session(
        &self,
        session_id: Option<&str>,
        hwnd: i64,
        region: Option<Rect>,
        client_region: bool,
        original_error: ErrorData,
    ) -> Result<crate::desktop_worker::HiddenDesktopCapture, ErrorData> {
        let Some(session_id) = session_id else {
            return Err(original_error);
        };
        let Some(hidden_desktop) = self.session_hidden_desktop_readback(session_id)? else {
            return Err(original_error);
        };
        for desktop_name in &hidden_desktop.desktop_names {
            match crate::desktop_worker::hidden_desktop_window_capture(
                desktop_name,
                hwnd,
                region,
                client_region,
            ) {
                Ok(capture) => return Ok(capture),
                Err(error) if hidden_worker_target_miss(&error) => {}
                Err(error) => return Err(error),
            }
        }
        Err(mcp_error(
            error_codes::TARGET_WINDOW_NOT_FOUND,
            format!("hidden desktop target hwnd {hwnd:#x} was not found in session {session_id}"),
        ))
    }

    fn capture_hidden_desktop_screenshot_to_file(
        &self,
        params: &CaptureScreenshotParams,
        session_id: Option<&str>,
        hwnd: i64,
        original_error: ErrorData,
    ) -> Result<CaptureScreenshotResponse, ErrorData> {
        let output_path = screenshot_output_path(&params.path)?;
        let format = screenshot_format_from_path(&output_path)?;
        ensure_screenshot_path_available(&output_path, params.overwrite)?;
        let captured = self.hidden_desktop_capture_for_session(
            session_id,
            hwnd,
            params.region,
            params.region.is_some(),
            original_error,
        )?;
        let bitmap_sha256 = sha256_hex(&captured.bitmap.bytes);
        write_screenshot_bitmap(
            params,
            output_path,
            format,
            captured.bitmap,
            captured.capture_backend,
            bitmap_sha256,
            Some(captured.context),
            None,
        )
    }

    #[cfg(windows)]
    async fn capture_cdp_target_screenshot_to_file(
        &self,
        params: &CaptureScreenshotParams,
        session_id: &str,
        window_hwnd: i64,
        cdp_target_id: &str,
    ) -> Result<CaptureScreenshotResponse, ErrorData> {
        validate_cdp_target_id(cdp_target_id)?;
        if let Some(region) = params.region {
            validate_screenshot_region(region)?;
        }
        let owner =
            self.cdp_target_owner_for_readback("capture_screenshot", session_id, cdp_target_id)?;
        if let Some(owner) = owner.as_ref()
            && owner.window_hwnd != window_hwnd
        {
            return Err(mcp_error(
                error_codes::ACTION_TARGET_INVALID,
                format!(
                    "capture_screenshot refused target {cdp_target_id:?}: owner window {:#x} does not match bound window {:#x}",
                    owner.window_hwnd, window_hwnd
                ),
            ));
        }
        let endpoint = synapse_a11y::endpoint_for_window(window_hwnd)
            .or_else(|| owner.as_ref().map(|owner| owner.endpoint.clone()))
            .ok_or_else(|| {
                mcp_error(
                    error_codes::ACTION_TARGET_INVALID,
                    format!(
                        "capture_screenshot refused CDP target {cdp_target_id:?}: no raw CDP endpoint and no session-owned Chrome bridge owner row"
                    ),
                )
            })?;
        let output_path = screenshot_output_path(&params.path)?;
        let format = screenshot_format_from_path(&output_path)?;
        ensure_screenshot_path_available(&output_path, params.overwrite)?;
        let target_context = resolve_capture_target_window_context(window_hwnd).ok();
        if is_chrome_debugger_endpoint(&endpoint) {
            let captured = crate::chrome_debugger_bridge::capture_visible_tab(
                window_hwnd,
                cdp_target_id,
                owner.as_ref().and_then(|owner| owner.chrome_window_id),
            )
            .await
            .map_err(|error| {
                mcp_error(
                    error.code(),
                    format!(
                        "capture_screenshot popup-free Chrome bridge refused debugger screenshot/readback: {}",
                        error.detail()
                    ),
                )
            })?;
            if !cdp_target_ids_equal(&captured.target_id, cdp_target_id) {
                return Err(mcp_error(
                    error_codes::ACTION_POSTCONDITION_FAILED,
                    format!(
                        "capture_screenshot Chrome bridge returned target {:?} for requested target {:?}",
                        captured.target_id, cdp_target_id
                    ),
                ));
            }
            if let Some(expected_window_id) =
                owner.as_ref().and_then(|owner| owner.chrome_window_id)
                && captured.chrome_window_id != Some(expected_window_id)
            {
                return Err(mcp_error(
                    error_codes::ACTION_POSTCONDITION_FAILED,
                    format!(
                        "capture_screenshot Chrome bridge captured Chrome window {:?} for requested target {:?}, expected Chrome window {}",
                        captured.chrome_window_id, cdp_target_id, expected_window_id
                    ),
                ));
            }
            let bitmap = chrome_capture_visible_tab_data_url_to_bgra(
                &captured.image_data_url,
                params.region,
            )?;
            let bitmap_sha256 = sha256_hex(&bitmap.bytes);
            tracing::info!(
                code = "CDP_TARGET_SCREENSHOT_CAPTURED",
                session_id = %session_id,
                hwnd = window_hwnd,
                endpoint = %endpoint,
                cdp_target_id = %captured.target_id,
                tab_id = captured.tab_id,
                chrome_window_id = captured.chrome_window_id.unwrap_or_default(),
                before_active = captured.before_active,
                active_for_capture = captured.active_for_capture,
                restored_previous_active = captured.restored_previous_active,
                image_data_url_len = captured.image_data_url_len,
                capture_attempt_count = captured.capture_attempt_count,
                capture_attempts = ?captured.capture_attempts,
                output_path = %output_path.display(),
                "readback=chrome.debugger.Page.captureScreenshot outcome=screenshot_bitmap_decoded"
            );
            return write_screenshot_bitmap(
                params,
                output_path,
                format,
                bitmap,
                "chrome_debugger_page_capture_screenshot_bgra",
                bitmap_sha256,
                None,
                None,
            );
        }
        let page_bitmap =
            synapse_a11y::cdp_capture_page_bgra(&endpoint, cdp_target_id, params.region)
                .await
                .map_err(|error| {
                    mcp_error(
                        error.code(),
                        format!(
                            "capture_screenshot raw CDP Page.captureScreenshot failed: {error}"
                        ),
                    )
                })?;
        let captured = cdp_page_bitmap_to_captured_bgra(page_bitmap, params.region)?;
        let bitmap_sha256 = sha256_hex(&captured.bytes);
        tracing::info!(
            code = "CDP_TARGET_SCREENSHOT_CAPTURED",
            session_id = %session_id,
            hwnd = window_hwnd,
            endpoint = %endpoint,
            cdp_target_id = %cdp_target_id,
            output_path = %output_path.display(),
            "readback=Page.captureScreenshot outcome=screenshot_bitmap_decoded"
        );
        write_screenshot_bitmap(
            params,
            output_path,
            format,
            captured,
            "raw_cdp_page_capture_screenshot_bgra",
            bitmap_sha256,
            target_context,
            None,
        )
    }

    #[cfg(not(windows))]
    async fn capture_cdp_target_screenshot_to_file(
        &self,
        _params: &CaptureScreenshotParams,
        _session_id: &str,
        _window_hwnd: i64,
        _cdp_target_id: &str,
    ) -> Result<CaptureScreenshotResponse, ErrorData> {
        Err(mcp_error(
            error_codes::A11Y_NOT_AVAILABLE,
            "capture_screenshot CDP target screenshots are only available on Windows in this build",
        ))
    }

    async fn browser_screenshot_impl(
        &self,
        params: &BrowserScreenshotParams,
        validation: &BrowserScreenshotValidation,
        session_id: &str,
        window_hwnd: i64,
        cdp_target_id: &str,
    ) -> Result<BrowserScreenshotResponse, ErrorData> {
        if !cdp_target_id.starts_with("chrome-tab:") {
            return Err(mcp_error(
                error_codes::ACTION_TARGET_INVALID,
                format!(
                    "browser_screenshot requires a normal Chrome bridge target shaped like chrome-tab:<id>; got {cdp_target_id:?}"
                ),
            ));
        }
        ensure_screenshot_path_available(&validation.output_path, params.overwrite)?;
        let bridge_payload = browser_screenshot_bridge_payload(params, validation.format)?;
        // #1359: serialize the brief foreground-capture critical section so
        // concurrent browser_screenshot captures (multi-agent / batched) cannot
        // interleave their Chrome-window activation and corrupt each other's
        // foreground-restore tracking — which surfaced as a spurious
        // "physical foreground drifted ... during capture" failure. This never
        // fights the human: each capture still restores the human's foreground,
        // and a genuine human-contention drift still fails loud (we never
        // re-steal focus from an actively-used human window). The lock only
        // makes concurrent agent captures queue.
        let _foreground_serialization = BROWSER_SCREENSHOT_FOREGROUND_LOCK.lock().await;
        let foreground_guard = prepare_browser_screenshot_foreground(window_hwnd)?;
        let mut captured_result = crate::chrome_debugger_bridge::page_screenshot(
            window_hwnd,
            cdp_target_id,
            bridge_payload.clone(),
        )
        .await
        .map_err(|error| {
            mcp_error(
                error.code(),
                format!(
                    "browser_screenshot Chrome bridge capture failed: {}",
                    error.detail()
                ),
            )
        });
        if let Err(error) = &captured_result
            && browser_screenshot_bridge_disconnected(error)
        {
            captured_result = browser_screenshot_retry_after_bridge_disconnect(
                window_hwnd,
                cdp_target_id,
                bridge_payload.clone(),
                error,
            )
            .await;
        }
        let foreground_readback = finish_browser_screenshot_foreground(
            window_hwnd,
            foreground_guard,
            captured_result.as_ref().err(),
        )?;
        let captured = match captured_result {
            Ok(captured) => captured,
            Err(error) => return Err(error),
        };
        if !cdp_target_ids_equal(&captured.target_id, cdp_target_id) {
            return Err(mcp_error(
                error_codes::ACTION_POSTCONDITION_FAILED,
                format!(
                    "browser_screenshot Chrome bridge returned target {:?} for requested target {:?}",
                    captured.target_id, cdp_target_id
                ),
            ));
        }
        tracing::info!(
            code = "BROWSER_SCREENSHOT_BRIDGE_CAPTURED",
            session_id = %session_id,
            hwnd = window_hwnd,
            cdp_target_id = %captured.target_id,
            tab_id = captured.tab_id,
            chrome_window_id = captured.chrome_window_id.unwrap_or_default(),
            before_active = captured.before_active,
            active_for_capture = captured.active_for_capture,
            restored_previous_active = captured.restored_previous_active,
            required_foreground = foreground_readback.required_foreground || captured.required_foreground,
            human_os_foreground_before_hwnd = foreground_readback.before_hwnd.unwrap_or_default(),
            human_os_foreground_capture_hwnd = foreground_readback.capture_hwnd.unwrap_or_default(),
            human_os_foreground_after_restore_hwnd = foreground_readback.after_restore_hwnd.unwrap_or_default(),
            restored_human_os_foreground = foreground_readback.restored_human_os_foreground,
            capture_attempt_count = captured.capture_attempt_count,
            capture_attempts = ?captured.capture_attempts,
            tile_count = captured.tile_count,
            output_path = %validation.output_path.display(),
            "readback=chrome.tabs.captureVisibleTab outcome=bridge_tiles_returned"
        );
        let bitmap =
            stitch_browser_screenshot_tiles(&captured, validation.format, params.omit_background)?;
        let bitmap_sha256 = sha256_hex(&bitmap.bytes);
        let write_params = CaptureScreenshotParams {
            path: params.path.clone(),
            region: Some(bitmap.region),
            window_hwnd: None,
            overwrite: params.overwrite,
            max_pixels: params.max_pixels,
            max_long_edge: params.max_long_edge,
        };
        let screenshot = write_screenshot_bitmap_with_quality(
            &write_params,
            validation.output_path.clone(),
            validation.format,
            bitmap,
            "chrome_tabs_capture_visible_tab_stitched_bgra",
            bitmap_sha256,
            None,
            params.quality,
            None,
        )?;
        let page_region = browser_screenshot_page_region(captured.clip_css)?;
        Ok(BrowserScreenshotResponse {
            path: screenshot.path,
            format: screenshot.format,
            capture_backend: screenshot.capture_backend,
            scope: params.scope,
            page_region,
            width: screenshot.width,
            height: screenshot.height,
            native_width: screenshot.native_width,
            native_height: screenshot.native_height,
            scale: screenshot.scale,
            bytes_written: screenshot.bytes_written,
            bitmap_sha256: screenshot.bitmap_sha256,
            cdp_target_id: captured.target_id,
            tab_id: captured.tab_id,
            chrome_window_id: captured.chrome_window_id,
            url: redact_url_for_public_readback(&captured.url),
            title: captured.title,
            device_pixel_ratio: captured.device_pixel_ratio,
            viewport_width_css: captured.viewport_width_css,
            viewport_height_css: captured.viewport_height_css,
            scroll_width_css: captured.scroll_width_css,
            scroll_height_css: captured.scroll_height_css,
            tile_count: captured.tile_count,
            mask_count: captured.mask_count,
            omit_background: captured.omit_background,
            required_foreground: foreground_readback.required_foreground || captured.required_foreground,
            human_os_foreground_before_hwnd: foreground_readback.before_hwnd,
            human_os_foreground_capture_hwnd: foreground_readback.capture_hwnd,
            human_os_foreground_after_restore_hwnd: foreground_readback.after_restore_hwnd,
            restored_human_os_foreground: foreground_readback.restored_human_os_foreground,
            backend_tier_used: captured.backend_tier_used,
            source_of_truth:
                "human OS foreground readback plus normal Chrome bridge chrome.scripting page metrics/masks/scroll and chrome.tabs.captureVisibleTab tiles stitched by synapse-mcp"
                    .to_owned(),
            degradation_code: None,
            fallback_metadata_source: None,
            fallback_reason: None,
        })
    }

    async fn browser_pdf_impl(
        &self,
        params: &BrowserPdfParams,
        validation: &BrowserPdfValidation,
        window_hwnd: i64,
        cdp_target_id: &str,
    ) -> Result<BrowserPdfResponse, ErrorData> {
        if !cdp_target_id.starts_with("chrome-tab:") {
            return Err(mcp_error(
                error_codes::ACTION_TARGET_INVALID,
                format!(
                    "browser_pdf requires a normal Chrome bridge target shaped like chrome-tab:<id>; got {cdp_target_id:?}"
                ),
            ));
        }
        ensure_screenshot_path_available(&validation.output_path, params.overwrite)?;
        let bridge_payload = browser_pdf_bridge_payload(params);
        let captured =
            crate::chrome_debugger_bridge::page_pdf(window_hwnd, cdp_target_id, bridge_payload)
                .await
                .map_err(|error| {
                    mcp_error(
                        error.code(),
                        format!(
                            "browser_pdf Chrome bridge Page.printToPDF failed: {}",
                            error.detail()
                        ),
                    )
                })?;
        if !cdp_target_ids_equal(&captured.target_id, cdp_target_id) {
            return Err(mcp_error(
                error_codes::ACTION_POSTCONDITION_FAILED,
                format!(
                    "browser_pdf Chrome bridge returned target {:?} for requested target {:?}",
                    captured.target_id, cdp_target_id
                ),
            ));
        }
        let pdf_bytes = base64::engine::general_purpose::STANDARD
            .decode(captured.data_base64.as_bytes())
            .map_err(|error| {
                mcp_error(
                    error_codes::TOOL_INTERNAL_ERROR,
                    format!("browser_pdf could not decode Page.printToPDF base64: {error}"),
                )
            })?;
        if !pdf_bytes.starts_with(b"%PDF-") {
            return Err(mcp_error(
                error_codes::ACTION_POSTCONDITION_FAILED,
                "browser_pdf Page.printToPDF decoded bytes did not start with %PDF-",
            ));
        }
        let pdf_sha256 = sha256_hex(&pdf_bytes);
        let bytes_written = write_pdf_bytes(&validation.output_path, &pdf_bytes, params.overwrite)?;
        Ok(BrowserPdfResponse {
            path: validation.output_path.to_string_lossy().into_owned(),
            bytes_written,
            pdf_sha256,
            capture_backend: "chrome_debugger_page_print_to_pdf".to_owned(),
            cdp_target_id: captured.target_id,
            tab_id: captured.tab_id,
            chrome_window_id: captured.chrome_window_id,
            url: redact_url_for_public_readback(&captured.url),
            title: captured.title,
            landscape: captured.landscape,
            print_background: captured.print_background,
            display_header_footer: captured.display_header_footer,
            scale: captured.scale,
            paper_width: captured.paper_width,
            paper_height: captured.paper_height,
            margin_top: captured.margin_top,
            margin_bottom: captured.margin_bottom,
            margin_left: captured.margin_left,
            margin_right: captured.margin_right,
            page_ranges: captured.page_ranges,
            prefer_css_page_size: captured.prefer_css_page_size,
            required_foreground: false,
            backend_tier_used: captured.backend_tier_used,
            source_of_truth:
                "normal Chrome bridge narrow chrome.debugger Page.printToPDF lane returning base64 PDF bytes written by synapse-mcp"
                .to_owned(),
        })
    }

    #[cfg(windows)]
    async fn browser_downloads_impl(
        &self,
        session_id: &str,
        window_context: ForegroundContext,
        used_human_os_foreground_window: bool,
        validation: &BrowserDownloadsValidation,
    ) -> Result<BrowserDownloadsResponse, ErrorData> {
        if synapse_a11y::endpoint_for_window(window_context.hwnd).is_some() {
            return Err(mcp_error(
                error_codes::ACTION_TARGET_INVALID,
                format!(
                    "browser_downloads targets the normal Chrome extension bridge, but window {:#x} exposes a raw CDP debug endpoint; use the already-open normal Chrome profile",
                    window_context.hwnd
                ),
            ));
        }
        let bridge_payload = browser_downloads_bridge_payload(&validation.params);
        let captured =
            crate::chrome_debugger_bridge::downloads(window_context.hwnd, bridge_payload)
                .await
                .map_err(|error| {
                    mcp_error(
                        error.code(),
                        format!(
                            "browser_downloads Chrome bridge chrome.downloads failed: {}",
                            error.detail()
                        ),
                    )
                })?;
        let endpoint = captured
            .extension_id
            .as_deref()
            .map(chrome_debugger_endpoint)
            .unwrap_or_else(chrome_debugger_default_endpoint);
        let selected_item = captured
            .selected_item
            .clone()
            .map(browser_download_entry_from_bridge);
        let items = captured
            .items
            .into_iter()
            .map(browser_download_entry_from_bridge)
            .collect::<Vec<_>>();
        let events = captured
            .events
            .into_iter()
            .map(browser_download_event_from_bridge)
            .collect::<Vec<_>>();
        let mut response = BrowserDownloadsResponse {
            session_id: session_id.to_owned(),
            operation: validation.params.operation,
            window_hwnd: window_context.hwnd,
            transport: "chrome_downloads_extension".to_owned(),
            endpoint,
            chrome_window_id: None,
            chrome_window_focused: None,
            chrome_window_state: None,
            used_human_os_foreground_window,
            condition_met: captured.condition_met,
            timed_out: captured.timed_out,
            elapsed_ms: captured.elapsed_ms,
            timeout_ms: captured.timeout_ms,
            returned: captured.returned,
            event_count: captured.event_count,
            next_event_cursor: captured.next_event_cursor,
            selected_item,
            items,
            events,
            saved_path: None,
            saved_bytes: None,
            saved_sha256: None,
            moved_file: false,
            required_foreground: captured.required_foreground,
            backend_tier_used: if captured.backend_tier_used.is_empty() {
                "chrome_downloads_api".to_owned()
            } else {
                captured.backend_tier_used
            },
            source_of_truth:
                "normal Chrome bridge chrome.downloads event/search readback plus daemon filesystem metadata and SHA-256 for save/move"
                    .to_owned(),
        };
        if matches!(
            validation.params.operation,
            BrowserDownloadsOperation::Save | BrowserDownloadsOperation::Move
        ) {
            let output_path = validation.output_path.as_ref().ok_or_else(|| {
                mcp_error(
                    error_codes::TOOL_INTERNAL_ERROR,
                    "browser_downloads save/move validation did not retain output path",
                )
            })?;
            let selected = response.selected_item.as_ref().ok_or_else(|| {
                mcp_error(
                    error_codes::ACTION_POSTCONDITION_FAILED,
                    "browser_downloads save/move found no matching completed download",
                )
            })?;
            if selected.state != "complete" {
                return Err(mcp_error(
                    error_codes::ACTION_POSTCONDITION_FAILED,
                    format!(
                        "browser_downloads save/move requires state=complete; selected download {} state={}",
                        selected.id, selected.state
                    ),
                ));
            }
            let source_path = browser_download_source_path(selected)?;
            let moved = validation.params.operation == BrowserDownloadsOperation::Move;
            let (saved_bytes, saved_sha256) = copy_or_move_download_file(
                &source_path,
                output_path,
                validation.params.overwrite,
                moved,
            )?;
            response.saved_path = Some(output_path.to_string_lossy().into_owned());
            response.saved_bytes = Some(saved_bytes);
            response.saved_sha256 = Some(saved_sha256);
            response.moved_file = moved;
        }
        Ok(response)
    }

    #[cfg(not(windows))]
    async fn browser_downloads_impl(
        &self,
        _session_id: &str,
        _window_context: ForegroundContext,
        _used_human_os_foreground_window: bool,
        _validation: &BrowserDownloadsValidation,
    ) -> Result<BrowserDownloadsResponse, ErrorData> {
        Err(mcp_error(
            error_codes::A11Y_NOT_AVAILABLE,
            "browser_downloads is only available on Windows in this build",
        ))
    }

    fn hidden_desktop_pip_frame_to_file(
        &self,
        params: &HiddenDesktopPipFrameParams,
        watched_session_id: &str,
    ) -> Result<HiddenDesktopPipFrameResponse, ErrorData> {
        let status = self.session_status_impl(watched_session_id)?;
        let lifecycle = status
            .session
            .as_ref()
            .map(|session| session.registry.lifecycle.clone());
        if lifecycle.as_deref() == Some("closed") {
            return Ok(hidden_desktop_pip_ended_response(
                params,
                watched_session_id,
                lifecycle,
                "watched_session_closed",
            ));
        }
        if !status.found {
            return Err(mcp_error(
                error_codes::HTTP_SESSION_INVALID,
                format!(
                    "hidden_desktop_pip_frame watched_session_id {watched_session_id:?} is unknown"
                ),
            ));
        }
        let original_error = mcp_error(
            error_codes::TARGET_WINDOW_NOT_FOUND,
            format!(
                "hidden_desktop_pip_frame target hwnd {:#x} was not found for watched session {watched_session_id:?}",
                params.window_hwnd
            ),
        );
        let screenshot_params = CaptureScreenshotParams {
            path: params.path.clone(),
            region: params.region,
            window_hwnd: Some(params.window_hwnd),
            overwrite: params.overwrite,
            max_pixels: None,
            max_long_edge: None,
        };
        let screenshot = self.capture_hidden_desktop_screenshot_to_file(
            &screenshot_params,
            Some(watched_session_id),
            params.window_hwnd,
            original_error,
        )?;
        let hidden_desktop = self
            .session_hidden_desktop_readback(watched_session_id)?
            .ok_or_else(|| {
                mcp_error(
                    error_codes::TARGET_WINDOW_NOT_FOUND,
                    format!(
                        "hidden_desktop_pip_frame watched session {watched_session_id:?} lost its session-owned hidden desktop resources after capture"
                    ),
                )
            })?;
        Ok(HiddenDesktopPipFrameResponse {
            stream_status: HiddenDesktopPipStreamStatus::Frame,
            watched_session_id: watched_session_id.to_owned(),
            watched_session_lifecycle: lifecycle,
            watched_window_hwnd: params.window_hwnd,
            viewer_surface: "mcp_file_frame".to_owned(),
            read_only: true,
            input_forwarding: "none".to_owned(),
            desktop_names: hidden_desktop.desktop_names,
            launch_pids: hidden_desktop.launch_pids,
            resource_count: hidden_desktop.resource_count,
            ended_reason: None,
            path: Some(screenshot.path),
            format: Some(screenshot.format),
            capture_backend: Some(screenshot.capture_backend),
            region: Some(screenshot.region),
            width: Some(screenshot.width),
            height: Some(screenshot.height),
            bytes_written: Some(screenshot.bytes_written),
            bitmap_sha256: Some(screenshot.bitmap_sha256),
            foreground: screenshot.foreground,
        })
    }

    #[cfg(windows)]
    fn read_text_hidden_desktop(
        &self,
        params: &ReadTextParams,
        session_id: &str,
        hwnd: i64,
        original_error: ErrorData,
    ) -> Result<OcrResult, ErrorData> {
        if params.element_id.is_some() {
            return Err(original_error);
        }
        let effective_backend = effective_ocr_backend(params.backend)?;
        let (request_region, capture_region, client_region, original_error) = if let Some(region) =
            params.region
        {
            (region, Some(region), true, original_error)
        } else {
            let Some(snapshot) =
                self.hidden_desktop_snapshot_for_session(session_id, hwnd, 2, original_error)?
            else {
                return Err(mcp_error(
                    error_codes::TARGET_WINDOW_NOT_FOUND,
                    format!(
                        "hidden desktop target hwnd {hwnd:#x} was not found in session {session_id}"
                    ),
                ));
            };
            let mut input = hidden_desktop_input_from_worker_snapshot(
                snapshot.tree,
                snapshot.context.clone(),
                synapse_core::PerceptionMode::Auto,
            );
            let Some(focused) = input.focused.take() else {
                return Err(mcp_error(
                    error_codes::OCR_NO_TEXT,
                    "hidden desktop read_text found no focused element OCR region",
                ));
            };
            let window_region = Rect {
                x: focused
                    .bbox
                    .x
                    .saturating_sub(snapshot.context.window_bounds.x),
                y: focused
                    .bbox
                    .y
                    .saturating_sub(snapshot.context.window_bounds.y),
                w: focused.bbox.w,
                h: focused.bbox.h,
            };
            (
                window_region,
                Some(window_region),
                false,
                mcp_error(
                    error_codes::TARGET_WINDOW_NOT_FOUND,
                    format!(
                        "hidden desktop target hwnd {hwnd:#x} was lost before OCR capture in session {session_id}"
                    ),
                ),
            )
        };
        let captured = self.hidden_desktop_capture_for_session(
            Some(session_id),
            hwnd,
            capture_region,
            client_region,
            original_error,
        )?;
        let request = crate::m1::ResolvedReadTextRequest {
            region: request_region,
            capture_source: crate::m1::ReadTextCaptureSource::Window {
                hwnd,
                window_region: captured.capture_region,
            },
            requested_backend: params.backend,
            effective_backend,
            lang_hint: params.lang_hint.clone(),
            synthetic: false,
            require_text: false,
        };
        self.read_text_request_with_captured_bitmap(
            request,
            CapturedOcrBitmap {
                bitmap: captured.bitmap,
                capture_source: "window",
                capture_backend: captured.capture_backend,
                capture_hwnd: Some(hwnd),
                capture_region: captured.capture_region,
            },
        )
    }

    #[cfg(not(windows))]
    fn read_text_hidden_desktop(
        &self,
        _params: &ReadTextParams,
        _session_id: &str,
        _hwnd: i64,
        original_error: ErrorData,
    ) -> Result<OcrResult, ErrorData> {
        Err(original_error)
    }

    #[tool(
        description = "Open a visible Chromium tab in the background using raw CDP Target.createTarget(background=true) or the installed normal Chrome bridge chrome.tabs.create(active=false), bind it to this MCP session, and return target-table readback. Requires an explicit browser window_hwnd or an existing session target; it never uses the human's current foreground as a fallback."
    )]
    pub async fn cdp_open_tab(
        &self,
        params: Parameters<CdpOpenTabParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<CdpOpenTabResponse>, ErrorData> {
        const TOOL: &str = "cdp_open_tab";
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = TOOL,
            "tool.invocation kind=cdp_open_tab"
        );
        let session_id = require_target_session_id(&request_context)?;
        self.cdp_open_tab_for_session(params.0, &session_id)
            .await
            .map(Json)
    }

    pub(super) async fn cdp_open_tab_for_session(
        &self,
        params: CdpOpenTabParams,
        session_id: &str,
    ) -> Result<CdpOpenTabResponse, ErrorData> {
        const TOOL: &str = "cdp_open_tab";
        validate_cdp_tab_url(&params.url)?;
        let window_hwnd = self.resolve_cdp_context_window(session_id, params.window_hwnd)?;
        let window_context = validate_target_window_context(window_hwnd)?;
        let window_title = window_context.window_title.clone();
        let process_name = window_context.process_name.clone();
        let endpoint = cdp_endpoint_for_action_log(window_hwnd);
        let request_details = json!({
            "session_id": session_id,
            "window_hwnd": window_hwnd,
            "endpoint": &endpoint,
            "requested_url": &params.url,
            "background": true,
            "required_foreground": false,
            "expected_window_bounds": &window_context.window_bounds,
        });
        self.audit_action_started_with_details_for_session(TOOL, &request_details, session_id)?;
        let result = self
            .cdp_open_tab_impl(
                session_id,
                window_hwnd,
                window_context.window_bounds,
                &params.url,
                &window_title,
                &process_name,
            )
            .await;
        self.audit_action_result_for_session(TOOL, &result, session_id)?;
        result
    }

    #[tool(
        description = "Close a CDP tab created by Synapse cdp_open_tab. Same-session closes use the in-memory owner row; after daemon/session churn, close authority can be restored only from the durable Synapse-created owner row when this session also holds an exact target_claim for the CDP target and creator/client lineage matches. Refuses unowned, unclaimed, ambiguous, unrelated, or human-foreground-fallback targets; it never activates the browser."
    )]
    pub async fn cdp_close_tab(
        &self,
        params: Parameters<CdpCloseTabParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<CdpCloseTabResponse>, ErrorData> {
        const TOOL: &str = "cdp_close_tab";
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = TOOL,
            "tool.invocation kind=cdp_close_tab"
        );
        let session_id = require_target_session_id(&request_context)?;
        let request_details = json!({
            "session_id": &session_id,
            "cdp_target_id": &params.0.cdp_target_id,
            "required_foreground": false,
        });
        if let Err(error) = validate_cdp_target_id(&params.0.cdp_target_id) {
            self.audit_action_denied_with_details_for_request(
                TOOL,
                &error,
                &request_details,
                &request_context,
            );
            return Err(error);
        }
        let (owner_key, owner) =
            match self.cdp_target_owner_for_close(&session_id, &params.0.cdp_target_id, None) {
                Ok(owner) => owner,
                Err(error) => {
                    self.audit_action_denied_with_details_for_request(
                        TOOL,
                        &error,
                        &request_details,
                        &request_context,
                    );
                    return Err(error);
                }
            };
        self.audit_action_started_with_details_for_session(
            TOOL,
            &json!({
                "session_id": &session_id,
                "window_hwnd": owner.window_hwnd,
                "endpoint": &owner.endpoint,
                "cdp_target_id": &params.0.cdp_target_id,
                "required_foreground": false,
            }),
            &session_id,
        )?;
        let result = self
            .cdp_close_tab_impl(&session_id, &params.0.cdp_target_id, &owner_key, owner)
            .await;
        self.audit_action_result_for_session(TOOL, &result, &session_id)?;
        result.map(Json)
    }

    #[tool(
        description = "Read the calling session's active browser tab target, or an explicit session-owned target, without navigation, activation, or debugger attach. Raw CDP uses Target.getTargets plus bounded Runtime.evaluate page-text and page-vitals readback; the normal Chrome bridge uses chrome.tabs.get plus content-script active-element/page-text/page-vitals readback where extension permissions allow. It never uses the human foreground tab as an implicit fallback. Page text is untrusted perceived web content; suspicious text is annotated in page_text. page_vitals reports document visibility plus Largest Contentful Paint entries from the page Performance Timeline without pretending hidden-tab LCP is valid."
    )]
    pub async fn cdp_target_info(
        &self,
        params: Parameters<CdpTargetInfoParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<CdpTargetInfoResponse>, ErrorData> {
        const TOOL: &str = "cdp_target_info";
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = TOOL,
            "tool.invocation kind=cdp_target_info"
        );
        let session_id = require_target_session_id(&request_context)?;
        let request_details = cdp_target_info_resolution_request_details(&session_id, &params.0);
        let (window_hwnd, cdp_target_id) = self.audit_cdp_target_resolution_result(
            TOOL,
            &session_id,
            &request_details,
            self.resolve_cdp_target_info_target(&session_id, &params.0),
        )?;
        let request_details = json!({
            "session_id": &session_id,
            "window_hwnd": window_hwnd,
            "cdp_target_id": &cdp_target_id,
            "required_foreground": false,
        });
        self.audit_action_started_with_details_for_session(TOOL, &request_details, &session_id)?;
        let result = self
            .cdp_target_info_impl(&session_id, window_hwnd, &cdp_target_id)
            .await;
        self.audit_action_result_for_session(TOOL, &result, &session_id)?;
        result.map(Json)
    }

    #[tool(
        description = "Ask the installed normal Chrome bridge extension to reload itself in the background via chrome.runtime.reload(), then wait for a new authenticated bridge host registration. This never opens chrome://extensions, never activates Chrome, and fails closed with CHROME_BRIDGE_EXTENSION_STALE when the loaded worker does not advertise reloadSelf."
    )]
    pub async fn cdp_bridge_reload(
        &self,
        params: Parameters<CdpBridgeReloadParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<CdpBridgeReloadResponse>, ErrorData> {
        const TOOL: &str = "cdp_bridge_reload";
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = TOOL,
            "tool.invocation kind=cdp_bridge_reload"
        );
        let session_id = require_target_session_id(&request_context)?;
        let wait_timeout_ms =
            crate::chrome_debugger_bridge::validate_reload_wait_timeout(params.0.wait_timeout_ms)
                .map_err(|error| mcp_error(error.code(), error.detail().to_owned()))?;
        let request_details = json!({
            "session_id": &session_id,
            "wait_timeout_ms": wait_timeout_ms,
            "required_foreground": false,
            "trigger": "chrome.runtime.reload",
        });
        self.audit_action_started_with_details_for_session(TOOL, &request_details, &session_id)?;
        let result = crate::chrome_debugger_bridge::reload_bridge(wait_timeout_ms)
            .await
            .map(|reload| chrome_bridge_reload_response(&session_id, wait_timeout_ms, reload))
            .map_err(|error| mcp_error(error.code(), error.detail().to_owned()));
        self.audit_action_result_for_session(TOOL, &result, &session_id)?;
        result.map(Json)
    }

    #[tool(
        description = "List or manage tabs in an already-open Chromium browser window through the normal Chrome bridge without debugger attach or OS foreground input (#1298/#1188). operation=list enumerates tabs and is the default; operation=select binds a listed tab as this MCP session target; operation=activate makes a listed tab active/highlighted inside its existing Chrome window without session target binding and fails closed if the requested Chrome window becomes the human OS foreground during the background activation; operation=new opens a background tab through the existing cdp_open_tab path; operation=close closes a same-session-owned tab through cdp_close_tab ownership checks. Human foreground is only an explicit discovery source for list/select when no session target/window is supplied; activate/new/close require an active/explicit browser context. Each row includes a ready-to-pass set_target payload with kind=cdp and cdp_target_id=chrome-tab:<id>."
    )]
    pub async fn browser_tabs(
        &self,
        params: Parameters<BrowserTabsParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<BrowserTabsResponse>, ErrorData> {
        const TOOL: &str = "browser_tabs";
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = TOOL,
            "tool.invocation kind=browser_tabs"
        );
        let session_id = require_target_session_id(&request_context)?;
        let params = validate_browser_tabs_params(params.0)?;
        let allow_human_foreground_discovery = matches!(
            params.operation,
            BrowserTabsOperation::List | BrowserTabsOperation::Select
        );
        if !allow_human_foreground_discovery
            && params.window_hwnd.is_none()
            && self.session_target(Some(&session_id))?.is_none()
        {
            return Err(mcp_error(
                error_codes::TARGET_NOT_SET,
                format!(
                    "{TOOL} operation={:?} requires window_hwnd or an active session target; refusing to use the human foreground browser as an implicit mutation target",
                    params.operation
                ),
            ));
        }
        let window_resolution = self.resolve_browser_tabs_window_context(
            TOOL,
            &session_id,
            params.window_hwnd,
            allow_human_foreground_discovery,
        )?;
        let window_context = window_resolution.context.clone();
        if !allow_human_foreground_discovery && window_resolution.used_human_os_foreground_window {
            return Err(mcp_error(
                error_codes::TARGET_NOT_SET,
                format!(
                    "{TOOL} operation={:?} refused human foreground fallback; pass window_hwnd or set a session target first",
                    params.operation
                ),
            ));
        }
        let request_details = json!({
            "session_id": &session_id,
            "operation": params.operation,
            "window_hwnd": window_context.hwnd,
            "window_title": &window_context.window_title,
            "process_name": &window_context.process_name,
            "used_human_os_foreground_window": window_resolution.used_human_os_foreground_window,
            "window_discovery_source": window_resolution.discovery_source,
            "chromium_window_candidate_count": window_resolution.chromium_window_candidate_count,
            "required_foreground": false,
            "no_debugger_attach": true,
            "requested_cdp_target": cdp_target_id_audit_ref(params.cdp_target_id.as_deref()),
            "requested_url_len": params.url.as_deref().map(str::len),
        });
        self.audit_action_started_with_details_for_session(TOOL, &request_details, &session_id)?;
        let result = self
            .browser_tabs_dispatch(
                &session_id,
                window_context,
                window_resolution.used_human_os_foreground_window,
                &params,
            )
            .await;
        self.audit_action_result_for_session(TOOL, &result, &session_id)?;
        result.map(Json)
    }

    #[tool(
        description = "Public browser navigation facade for an owned already-open Chrome tab. operation=navigate/reload/back/forward routes through the same target-scoped background navigation engine as cdp_navigate_tab, but exposes a strict facade schema, never uses the human foreground tab as an implicit target, and returns separate URL/title/readyState readback metadata."
    )]
    pub async fn browser_nav(
        &self,
        params: Parameters<BrowserNavParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<BrowserNavResponse>, ErrorData> {
        const TOOL: &str = "browser_nav";
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = TOOL,
            "tool.invocation kind=browser_nav"
        );
        let session_id = require_target_session_id(&request_context)?;
        let source_id = browser_nav_source_id(&params.0);
        let validated = validate_browser_nav_params(params.0)?;
        let request_details = json!({
            "session_id": &session_id,
            "operation": validated.params.operation.as_str(),
            "window_hwnd": validated.params.window_hwnd,
            "requested_cdp_target": cdp_target_id_audit_ref(validated.params.cdp_target_id.as_deref()),
            "requested_url": validated.requested_url.as_deref(),
            "wait_timeout_ms": validated.wait_timeout_ms,
            "ignore_cache": validated.ignore_cache,
            "required_foreground": false,
            "source_of_truth": BROWSER_NAV_SOURCE_OF_TRUTH,
            "readback_source_of_truth": BROWSER_NAV_READBACK_SOURCE_OF_TRUTH,
        });
        self.audit_action_started_with_details_for_session(TOOL, &request_details, &session_id)?;
        let result = async {
            let (window_hwnd, cdp_target_id) = self
                .resolve_cdp_tab_mutation_target(
                    TOOL,
                    &session_id,
                    validated.params.window_hwnd,
                    validated.params.cdp_target_id.as_deref(),
                )
                .map_err(|error| {
                    browser_nav_delegate_error(
                        validated.params.operation,
                        source_id.clone(),
                        error,
                        "select or open a tab with browser_tabs first, then retry browser_nav with that cdp_target_id or active session target",
                    )
                })?;
            self.cdp_navigate_tab_impl(
                TOOL,
                &session_id,
                window_hwnd,
                &cdp_target_id,
                validated.action,
                validated.requested_url.as_deref(),
                validated.wait_timeout_ms,
                validated.ignore_cache,
            )
            .await
            .map(|navigation| BrowserNavResponse {
                operation: validated.params.operation,
                source_of_truth: BROWSER_NAV_SOURCE_OF_TRUTH.to_owned(),
                readback_source_of_truth: BROWSER_NAV_READBACK_SOURCE_OF_TRUTH.to_owned(),
                navigation: redact_cdp_navigate_tab_response_urls(navigation),
            })
            .map_err(|error| {
                browser_nav_delegate_error(
                    validated.params.operation,
                    source_id,
                    error,
                    "inspect the target tab with browser_tabs operation=list, verify the URL/wait budget, and retry with an owned target",
                )
            })
        }
        .await;
        self.audit_action_result_for_session(TOOL, &result, &session_id)?;
        result.map(Json)
    }

    #[tool(
        description = "Explicitly adopt the active tab from an already-open Chromium browser window as this MCP session's active CDP target (#1298). This is the consented handoff path for the user's existing authenticated foreground tab: it lists tabs through the normal Chrome bridge, selects exactly one active tab in the requested/foreground window, and binds that chrome-tab:<id> with set_target semantics. It never creates, closes, navigates, activates, foregrounds, or debugger-attaches; adopted user tabs are drivable but are not owned for cdp_close_tab."
    )]
    pub async fn browser_adopt_active_tab(
        &self,
        params: Parameters<BrowserAdoptActiveTabParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<BrowserAdoptActiveTabResponse>, ErrorData> {
        const TOOL: &str = "browser_adopt_active_tab";
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = TOOL,
            "tool.invocation kind=browser_adopt_active_tab"
        );
        let session_id = require_target_session_id(&request_context)?;
        let window_resolution = self.resolve_browser_tabs_window_context(
            TOOL,
            &session_id,
            params.0.window_hwnd,
            false,
        )?;
        let window_context = window_resolution.context;
        let used_human_os_foreground_window = window_resolution.used_human_os_foreground_window;
        let request_details = json!({
            "session_id": &session_id,
            "window_hwnd": window_context.hwnd,
            "window_title": &window_context.window_title,
            "process_name": &window_context.process_name,
            "used_human_os_foreground_window": used_human_os_foreground_window,
            "window_discovery_source": &window_resolution.discovery_source,
            "chromium_window_candidate_count": window_resolution.chromium_window_candidate_count,
            "required_foreground": false,
            "no_debugger_attach": true,
            "mutation": "session_target_bind_only",
        });
        self.audit_action_started_with_details_for_session(TOOL, &request_details, &session_id)?;
        let result = self
            .browser_adopt_active_tab_impl(
                &session_id,
                window_context,
                used_human_os_foreground_window,
            )
            .await;
        self.audit_action_result_for_session(TOOL, &result, &session_id)?;
        result.map(Json)
    }

    #[tool(
        description = "Navigate, reload, back, or forward the calling session's active browser tab target in the background. Raw CDP uses Page.navigate/Page.reload/Page.navigateToHistoryEntry; the normal Chrome extension bridge uses chrome.tabs without debugger attach. Requires an active session CDP target or a target owned by this session; never uses the human foreground tab as an implicit fallback and returns separate URL/title/readback metadata."
    )]
    pub async fn cdp_navigate_tab(
        &self,
        params: Parameters<CdpNavigateTabParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<CdpNavigateTabResponse>, ErrorData> {
        const TOOL: &str = "cdp_navigate_tab";
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = TOOL,
            "tool.invocation kind=cdp_navigate_tab"
        );
        let session_id = require_target_session_id(&request_context)?;
        let requested_url = validate_cdp_navigation_params(&params.0)?;
        let wait_timeout_ms = validate_cdp_navigation_wait_timeout(params.0.wait_timeout_ms)?;
        let request_details = cdp_navigate_resolution_request_details(
            &session_id,
            &params.0,
            requested_url.as_deref(),
            wait_timeout_ms,
        );
        let (window_hwnd, cdp_target_id) = self.audit_cdp_target_resolution_result(
            TOOL,
            &session_id,
            &request_details,
            self.resolve_cdp_navigation_target(&session_id, &params.0),
        )?;
        let request_details = json!({
            "session_id": &session_id,
            "window_hwnd": window_hwnd,
            "cdp_target_id": &cdp_target_id,
            "action": params.0.action,
            "requested_url": requested_url.as_deref(),
            "wait_timeout_ms": wait_timeout_ms,
            "ignore_cache": params.0.ignore_cache.unwrap_or(false),
            "required_foreground": false,
        });
        self.audit_action_started_with_details_for_session(TOOL, &request_details, &session_id)?;
        let result = self
            .cdp_navigate_tab_impl(
                TOOL,
                &session_id,
                window_hwnd,
                &cdp_target_id,
                params.0.action,
                requested_url.as_deref(),
                wait_timeout_ms,
                params.0.ignore_cache.unwrap_or(false),
            )
            .await;
        self.audit_action_result_for_session(TOOL, &result, &session_id)?;
        result.map(Json)
    }

    #[tool(
        description = "Make the calling session's CDP tab the active tab in its own Chrome window with required_foreground=false (the background-safe Playwright bringToFront analogue). Raw CDP uses Target.activateTarget; the normal Chrome extension bridge uses chrome.tabs.update({active:true}) and separately reads the human OS foreground before/after, failing closed if the requested Chrome window becomes foreground. Requires an active session CDP target or a target owned by this session; never uses the human foreground tab as a fallback. Use this instead of injecting global keystrokes (e.g. SendKeys) through act_run_shell."
    )]
    pub async fn cdp_activate_tab(
        &self,
        params: Parameters<CdpActivateTabParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<CdpActivateTabResponse>, ErrorData> {
        const TOOL: &str = "cdp_activate_tab";
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = TOOL,
            "tool.invocation kind=cdp_activate_tab"
        );
        let session_id = require_target_session_id(&request_context)?;
        let wait_timeout_ms = validate_cdp_navigation_wait_timeout(params.0.wait_timeout_ms)?;
        let request_details =
            cdp_activate_resolution_request_details(&session_id, &params.0, wait_timeout_ms);
        let resolution = self.resolve_cdp_tab_mutation_target(
            TOOL,
            &session_id,
            params.0.window_hwnd,
            params.0.cdp_target_id.as_deref(),
        );
        let (window_hwnd, cdp_target_id) = self.audit_cdp_target_resolution_result(
            TOOL,
            &session_id,
            &request_details,
            resolution,
        )?;
        let request_details = json!({
            "session_id": &session_id,
            "window_hwnd": window_hwnd,
            "cdp_target_id": &cdp_target_id,
            "wait_timeout_ms": wait_timeout_ms,
            "required_foreground": false,
        });
        self.audit_action_started_with_details_for_session(TOOL, &request_details, &session_id)?;
        let result = self
            .cdp_activate_tab_impl(&session_id, window_hwnd, &cdp_target_id, wait_timeout_ms)
            .await;
        self.audit_action_result_for_session(TOOL, &result, &session_id)?;
        result.map(Json)
    }

    #[tool(
        description = "Evaluate JavaScript in the calling session's owned browser tab, returning the JSON value plus Runtime.RemoteObject-like type metadata read back from the same target. Raw CDP uses Runtime.evaluate / Runtime.callFunctionOn. The normal authenticated Chrome bridge supports page-scope evaluation through a narrow target-scoped chrome.debugger Runtime.evaluate lane in the already-open Chrome profile; element-scope evaluation still requires raw CDP because normal bridge element ids are DOM-path based. Page scope (default): `expression` is evaluated directly; pass `args` to invoke it as a function with those args. Element scope: pass `element_id` and a function `expression`, called Playwright-style as fn(element, ...args) via Runtime.callFunctionOn. Requires an active session target or an explicit cdp_target_id/element owned by this session; never uses an unrelated human foreground tab as a fallback. JS exceptions are surfaced loudly. Target-scoped: never changes tab activation or uses OS foreground input."
    )]
    pub async fn browser_evaluate(
        &self,
        params: Parameters<BrowserEvaluateParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<BrowserEvaluateResponse>, ErrorData> {
        const TOOL: &str = "browser_evaluate";
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = TOOL,
            "tool.invocation kind=browser_evaluate"
        );
        let session_id = require_target_session_id(&request_context)?;
        validate_browser_evaluate_params(&params.0)?;
        // Element-scoped evaluation derives its CDP target from the element id;
        // it must agree with any explicit cdp_target_id.
        let element = params
            .0
            .element_id
            .as_deref()
            .map(parse_browser_evaluate_element)
            .transpose()?;
        if let (Some((_, element_target)), Some(explicit)) =
            (element.as_ref(), params.0.cdp_target_id.as_deref())
            && !element_target.eq_ignore_ascii_case(explicit)
        {
            return Err(mcp_error(
                error_codes::ACTION_TARGET_INVALID,
                format!(
                    "browser_evaluate element_id resolves to CDP target {element_target:?} but cdp_target_id {explicit:?} was also supplied; they must match"
                ),
            ));
        }
        let resolution_target = params
            .0
            .cdp_target_id
            .clone()
            .or_else(|| element.as_ref().map(|(_, target)| target.clone()));
        let request_details = browser_evaluate_resolution_request_details(&session_id, &params.0);
        let resolution = self.resolve_cdp_tab_mutation_target(
            TOOL,
            &session_id,
            params.0.window_hwnd,
            resolution_target.as_deref(),
        );
        let (window_hwnd, cdp_target_id) = self.audit_cdp_target_resolution_result(
            TOOL,
            &session_id,
            &request_details,
            resolution,
        )?;
        let await_promise = params.0.await_promise.unwrap_or(true);
        let return_by_value = params.0.return_by_value.unwrap_or(true);
        let backend_node_id = element.as_ref().map(|(backend, _)| *backend);
        let args = params.0.args.clone().unwrap_or_default();
        let request_details = json!({
            "session_id": &session_id,
            "window_hwnd": window_hwnd,
            "cdp_target_id": &cdp_target_id,
            "scope": if backend_node_id.is_some() { "element" } else { "page" },
            "element_id": params.0.element_id,
            "expression_len": params.0.expression.len(),
            "arg_count": args.len(),
            "await_promise": await_promise,
            "return_by_value": return_by_value,
            "required_foreground": false,
        });
        self.audit_action_started_with_details_for_session(TOOL, &request_details, &session_id)?;
        let result = self
            .browser_evaluate_impl(
                &session_id,
                window_hwnd,
                &cdp_target_id,
                &params.0.expression,
                params.0.element_id.as_deref(),
                backend_node_id,
                &args,
                await_promise,
                return_by_value,
            )
            .await;
        self.audit_action_result_for_session(TOOL, &result, &session_id)?;
        result.map(Json)
    }

    #[tool(
        description = "Expose, read, or remove a Playwright-style page binding on the calling session's owned browser tab via raw CDP or the normal Chrome bridge's target-scoped chrome.debugger Runtime.addBinding / Runtime.bindingCalled / Runtime.removeBinding lane. operation=add installs a string-argument function on window and arms a persistent per-target event listener; operation=read returns the buffered payloads without mutating the page; operation=remove unsubscribes this Synapse runtime agent so future calls stop being delivered to the buffer. CDP removeBinding does not delete the JavaScript function object from existing page globals, so removal is verified by no new Runtime.bindingCalled delivery. Pair with browser_add_init_script when page code should re-wire helper wrappers across navigation. Target-scoped and background-safe: never activates the tab, never uses OS foreground input, and never falls back to the human foreground tab. Read-only payload capture, no host callback execution."
    )]
    pub async fn browser_expose_binding(
        &self,
        params: Parameters<BrowserExposeBindingParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<BrowserExposeBindingResponse>, ErrorData> {
        const TOOL: &str = "browser_expose_binding";
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = TOOL,
            "tool.invocation kind=browser_expose_binding"
        );
        let session_id = require_target_session_id(&request_context)?;
        let max_calls = validate_browser_expose_binding_params(&params.0)?;
        let request_details = json!({
            "session_id": &session_id,
            "window_hwnd": params.0.window_hwnd,
            "requested_cdp_target": cdp_target_id_audit_ref(params.0.cdp_target_id.as_deref()),
            "operation": params.0.operation,
            "name": &params.0.name,
            "execution_context_name": params.0.execution_context_name.as_deref(),
            "since_seq": params.0.since_seq,
            "max_calls": max_calls,
            "required_foreground": false,
            "phase": "target_resolution",
        });
        let resolution = self.resolve_cdp_tab_mutation_target(
            TOOL,
            &session_id,
            params.0.window_hwnd,
            params.0.cdp_target_id.as_deref(),
        );
        let (window_hwnd, cdp_target_id) = self.audit_cdp_target_resolution_result(
            TOOL,
            &session_id,
            &request_details,
            resolution,
        )?;
        let request_details = json!({
            "session_id": &session_id,
            "window_hwnd": window_hwnd,
            "cdp_target_id": &cdp_target_id,
            "operation": params.0.operation,
            "name": &params.0.name,
            "execution_context_name": params.0.execution_context_name.as_deref(),
            "since_seq": params.0.since_seq,
            "max_calls": max_calls,
            "required_foreground": false,
        });
        self.audit_action_started_with_details_for_session(TOOL, &request_details, &session_id)?;
        let result = self
            .browser_expose_binding_impl(
                &session_id,
                window_hwnd,
                &cdp_target_id,
                &params.0,
                max_calls,
            )
            .await;
        self.audit_action_result_for_session(TOOL, &result, &session_id)?;
        result.map(Json)
    }

    #[tool(
        description = "Add or remove a Playwright-style init script for the calling session's owned browser tab via raw CDP or the normal Chrome bridge's narrow target-scoped chrome.debugger Page.addScriptToEvaluateOnNewDocument / Page.removeScriptToEvaluateOnNewDocument lane. operation defaults to add: provide source, and the script runs before page scripts on every subsequent new document/navigation for that target. operation=remove requires the returned identifier. Target-scoped and background-safe: never activates the tab, never uses OS foreground input, and never falls back to the human foreground tab."
    )]
    pub async fn browser_add_init_script(
        &self,
        params: Parameters<BrowserAddInitScriptParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<BrowserAddInitScriptResponse>, ErrorData> {
        const TOOL: &str = "browser_add_init_script";
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = TOOL,
            "tool.invocation kind=browser_add_init_script"
        );
        let session_id = require_target_session_id(&request_context)?;
        validate_browser_add_init_script_params(&params.0)?;
        let request_details = json!({
            "session_id": &session_id,
            "window_hwnd": params.0.window_hwnd,
            "requested_cdp_target": cdp_target_id_audit_ref(params.0.cdp_target_id.as_deref()),
            "operation": params.0.operation,
            "source_len": params.0.source.as_deref().map(str::len),
            "identifier": params.0.identifier.as_deref(),
            "world_name": params.0.world_name.as_deref(),
            "include_command_line_api": params.0.include_command_line_api,
            "run_immediately": params.0.run_immediately,
            "required_foreground": false,
            "phase": "target_resolution",
        });
        let resolution = self.resolve_cdp_tab_mutation_target(
            TOOL,
            &session_id,
            params.0.window_hwnd,
            params.0.cdp_target_id.as_deref(),
        );
        let (window_hwnd, cdp_target_id) = self.audit_cdp_target_resolution_result(
            TOOL,
            &session_id,
            &request_details,
            resolution,
        )?;
        let request_details = json!({
            "session_id": &session_id,
            "window_hwnd": window_hwnd,
            "cdp_target_id": &cdp_target_id,
            "operation": params.0.operation,
            "source_len": params.0.source.as_deref().map(str::len),
            "identifier": params.0.identifier.as_deref(),
            "world_name": params.0.world_name.as_deref(),
            "include_command_line_api": params.0.include_command_line_api,
            "run_immediately": params.0.run_immediately,
            "required_foreground": false,
        });
        self.audit_action_started_with_details_for_session(TOOL, &request_details, &session_id)?;
        let result = self
            .browser_add_init_script_impl(&session_id, window_hwnd, &cdp_target_id, &params.0)
            .await;
        self.audit_action_result_for_session(TOOL, &result, &session_id)?;
        result.map(Json)
    }

    #[tool(
        description = "Inject a Playwright-style <script> tag into the calling session's owned current document from exactly one source: url, content, or local UTF-8 path. Raw CDP targets use Runtime.evaluate; normal Chrome bridge chrome-tab targets use the narrow target-scoped chrome.debugger Runtime.evaluate lane. URL sources wait for onload/onerror and surface load failures as structured MCP errors; inline/path sources append synchronously. Target-scoped and background-safe: never activates the tab, never uses OS foreground input, and never falls back to the human foreground tab."
    )]
    pub async fn browser_add_script_tag(
        &self,
        params: Parameters<BrowserAddScriptTagParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<BrowserAddTagResponse>, ErrorData> {
        const TOOL: &str = "browser_add_script_tag";
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = TOOL,
            "tool.invocation kind=browser_add_script_tag"
        );
        let session_id = require_target_session_id(&request_context)?;
        validate_browser_add_script_tag_params(&params.0)?;
        let source = resolve_browser_tag_source(
            TOOL,
            params.0.url.as_deref(),
            params.0.content.as_deref(),
            params.0.path.as_deref(),
        )?;
        let request_details = json!({
            "session_id": &session_id,
            "window_hwnd": params.0.window_hwnd,
            "requested_cdp_target": cdp_target_id_audit_ref(params.0.cdp_target_id.as_deref()),
            "source_kind": source.kind.as_str(),
            "requested_url": source.requested_url.as_deref(),
            "path": source.path.as_deref(),
            "content_len": source.content_len,
            "script_type": params.0.script_type.as_deref(),
            "required_foreground": false,
            "phase": "target_resolution",
        });
        let resolution = self.resolve_cdp_tab_mutation_target(
            TOOL,
            &session_id,
            params.0.window_hwnd,
            params.0.cdp_target_id.as_deref(),
        );
        let (window_hwnd, cdp_target_id) = self.audit_cdp_target_resolution_result(
            TOOL,
            &session_id,
            &request_details,
            resolution,
        )?;
        let request_details = json!({
            "session_id": &session_id,
            "window_hwnd": window_hwnd,
            "cdp_target_id": &cdp_target_id,
            "source_kind": source.kind.as_str(),
            "requested_url": source.requested_url.as_deref(),
            "path": source.path.as_deref(),
            "content_len": source.content_len,
            "script_type": params.0.script_type.as_deref(),
            "required_foreground": false,
        });
        self.audit_action_started_with_details_for_session(TOOL, &request_details, &session_id)?;
        let result = self
            .browser_add_tag_impl(
                &session_id,
                window_hwnd,
                &cdp_target_id,
                TOOL,
                BrowserTagKind::Script,
                &source,
                params.0.script_type.as_deref(),
            )
            .await;
        self.audit_action_result_for_session(TOOL, &result, &session_id)?;
        result.map(Json)
    }

    #[tool(
        description = "Inject a Playwright-style stylesheet into the calling session's owned current document from exactly one source: url, content, or local UTF-8 path. Raw CDP targets use Runtime.evaluate; normal Chrome bridge chrome-tab targets use the narrow target-scoped chrome.debugger Runtime.evaluate lane. URL sources create <link rel=stylesheet> and wait for onload/onerror; inline/path sources create <style>. Target-scoped and background-safe: never activates the tab, never uses OS foreground input, and never falls back to the human foreground tab."
    )]
    pub async fn browser_add_style_tag(
        &self,
        params: Parameters<BrowserAddStyleTagParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<BrowserAddTagResponse>, ErrorData> {
        const TOOL: &str = "browser_add_style_tag";
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = TOOL,
            "tool.invocation kind=browser_add_style_tag"
        );
        let session_id = require_target_session_id(&request_context)?;
        validate_browser_add_style_tag_params(&params.0)?;
        let source = resolve_browser_tag_source(
            TOOL,
            params.0.url.as_deref(),
            params.0.content.as_deref(),
            params.0.path.as_deref(),
        )?;
        let request_details = json!({
            "session_id": &session_id,
            "window_hwnd": params.0.window_hwnd,
            "requested_cdp_target": cdp_target_id_audit_ref(params.0.cdp_target_id.as_deref()),
            "source_kind": source.kind.as_str(),
            "requested_url": source.requested_url.as_deref(),
            "path": source.path.as_deref(),
            "content_len": source.content_len,
            "required_foreground": false,
            "phase": "target_resolution",
        });
        let resolution = self.resolve_cdp_tab_mutation_target(
            TOOL,
            &session_id,
            params.0.window_hwnd,
            params.0.cdp_target_id.as_deref(),
        );
        let (window_hwnd, cdp_target_id) = self.audit_cdp_target_resolution_result(
            TOOL,
            &session_id,
            &request_details,
            resolution,
        )?;
        let request_details = json!({
            "session_id": &session_id,
            "window_hwnd": window_hwnd,
            "cdp_target_id": &cdp_target_id,
            "source_kind": source.kind.as_str(),
            "requested_url": source.requested_url.as_deref(),
            "path": source.path.as_deref(),
            "content_len": source.content_len,
            "required_foreground": false,
        });
        self.audit_action_started_with_details_for_session(TOOL, &request_details, &session_id)?;
        let result = self
            .browser_add_tag_impl(
                &session_id,
                window_hwnd,
                &cdp_target_id,
                TOOL,
                BrowserTagKind::Style,
                &source,
                None,
            )
            .await;
        self.audit_action_result_for_session(TOOL, &result, &session_id)?;
        result.map(Json)
    }

    #[tool(
        description = "Public wait facade for the calling session's owned browser tab. operation=for_condition requires the nested wait spec and evaluates one typed condition: text, load_state, url, selector, function, request, or response. The facade rejects missing or extra operation fields before polling, delegates to the same target-scoped browser_wait_for runtime path, returns the full condition readback, and never activates Chrome, uses OS foreground input, or falls back to the human foreground tab."
    )]
    pub async fn browser_wait(
        &self,
        params: Parameters<BrowserWaitFacadeParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<BrowserWaitFacadeResponse>, ErrorData> {
        let params = params.0;
        let operation = params.operation;
        let source_id = browser_wait_facade_source_id(&params);
        let wait = validate_browser_wait_facade_params(params)?;
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = "browser_wait",
            operation = operation.as_str(),
            source_id = %source_id,
            "tool.invocation kind=browser_wait"
        );
        let response = self
            .browser_wait_for(Parameters(wait), request_context)
            .await
            .map_err(|error| {
                browser_wait_facade_delegate_error(
                    operation,
                    source_id.clone(),
                    error,
                    "bind the target tab and pass exactly one condition spec matching the wait condition",
                )
            })?;
        Ok(Json(BrowserWaitFacadeResponse {
            operation,
            source_of_truth: BROWSER_WAIT_FACADE_SOURCE_OF_TRUTH.to_owned(),
            readback_source_of_truth: BROWSER_WAIT_FACADE_READBACK_SOURCE_OF_TRUTH.to_owned(),
            wait: redact_browser_wait_response_urls(response.0),
        }))
    }

    #[tool(
        description = "Wait in the calling session's owned browser tab for one of seven predicates, selected by `condition` with the matching nested spec (#1348 — folds the former browser_wait_for_text/load_state/url/selector/function/request/response tools into one). condition=text waits for page text to appear/disappear or a plain timeout (spec `text`); condition=load_state waits for domcontentloaded/load/networkidle (spec `load_state`); condition=url waits until the tab URL matches an exact string/glob/regex (spec `url`); condition=selector waits for a Playwright-style selector to reach attached/visible/hidden/detached using the same engines/options as browser_locate (spec `selector`); condition=function polls a JavaScript predicate until truthy (spec `function`); condition=request/response wait for a captured network request/response matching url/method/status/resource_type predicates (spec `request`/`response`). Each spec object is exactly the former standalone tool's parameters. Raw CDP when available or the debugger-free normal Chrome bridge for chrome-tab:* targets. Timeouts return BROWSER_WAIT_TIMEOUT. Target-scoped and background-safe: never activates the tab, never uses OS foreground input, and never falls back to the human foreground tab. The response field matching `condition` carries that predicate's full result."
    )]
    pub async fn browser_wait_for(
        &self,
        params: Parameters<BrowserWaitParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<BrowserWaitResponse>, ErrorData> {
        let params = params.0;
        let condition = params.condition;
        let missing = |field: &str| {
            mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                format!(
                    "browser_wait_for condition={condition:?} requires the `{field}` spec object"
                ),
            )
        };
        match condition {
            BrowserWaitConditionKind::Text => {
                let spec = params.text.ok_or_else(|| missing("text"))?;
                let inner = self
                    .browser_wait_for_text_inner(Parameters(spec), request_context)
                    .await?;
                Ok(Json(BrowserWaitResponse {
                    condition,
                    text: Some(inner.0),
                    ..Default::default()
                }))
            }
            BrowserWaitConditionKind::LoadState => {
                let spec = params.load_state.ok_or_else(|| missing("load_state"))?;
                let inner = self
                    .browser_wait_for_load_state_inner(Parameters(spec), request_context)
                    .await?;
                Ok(Json(BrowserWaitResponse {
                    condition,
                    load_state: Some(inner.0),
                    ..Default::default()
                }))
            }
            BrowserWaitConditionKind::Url => {
                let spec = params.url.ok_or_else(|| missing("url"))?;
                let inner = self
                    .browser_wait_for_url_inner(Parameters(spec), request_context)
                    .await?;
                Ok(Json(BrowserWaitResponse {
                    condition,
                    url: Some(inner.0),
                    ..Default::default()
                }))
            }
            BrowserWaitConditionKind::Selector => {
                let spec = params.selector.ok_or_else(|| missing("selector"))?;
                let inner = self
                    .browser_wait_for_selector_inner(Parameters(spec), request_context)
                    .await?;
                Ok(Json(BrowserWaitResponse {
                    condition,
                    selector: Some(inner.0),
                    ..Default::default()
                }))
            }
            BrowserWaitConditionKind::Function => {
                let spec = params.function.ok_or_else(|| missing("function"))?;
                let inner = self
                    .browser_wait_for_function_inner(Parameters(spec), request_context)
                    .await?;
                Ok(Json(BrowserWaitResponse {
                    condition,
                    function: Some(inner.0),
                    ..Default::default()
                }))
            }
            BrowserWaitConditionKind::Request => {
                let spec = params.request.ok_or_else(|| missing("request"))?;
                let inner = self
                    .browser_wait_for_request_inner(Parameters(spec), request_context)
                    .await?;
                Ok(Json(BrowserWaitResponse {
                    condition,
                    request: Some(inner.0),
                    ..Default::default()
                }))
            }
            BrowserWaitConditionKind::Response => {
                let spec = params.response.ok_or_else(|| missing("response"))?;
                let inner = self
                    .browser_wait_for_response_inner(Parameters(spec), request_context)
                    .await?;
                Ok(Json(BrowserWaitResponse {
                    condition,
                    response: Some(inner.0),
                    ..Default::default()
                }))
            }
        }
    }

    /// Text/timeout wait predicate — internal lane for the unified
    /// `browser_wait_for` tool (#1348).
    pub async fn browser_wait_for_text_inner(
        &self,
        params: Parameters<BrowserWaitForParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<BrowserWaitForResponse>, ErrorData> {
        const TOOL: &str = "browser_wait_for";
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = TOOL,
            "tool.invocation kind=browser_wait_for"
        );
        let session_id = require_target_session_id(&request_context)?;
        let wait = validate_browser_wait_for_params(&params.0)?;
        let request_details = json!({
            "session_id": &session_id,
            "window_hwnd": params.0.window_hwnd,
            "requested_cdp_target": cdp_target_id_audit_ref(params.0.cdp_target_id.as_deref()),
            "state": wait.state,
            "text_len": wait.text.as_deref().map(str::len),
            "timeout_ms": wait.timeout_ms,
            "polling_interval_ms": wait.polling_interval_ms,
            "required_foreground": false,
            "phase": "target_resolution",
        });
        let resolution = self.resolve_cdp_tab_mutation_target(
            TOOL,
            &session_id,
            params.0.window_hwnd,
            params.0.cdp_target_id.as_deref(),
        );
        let (window_hwnd, cdp_target_id) = self.audit_cdp_target_resolution_result(
            TOOL,
            &session_id,
            &request_details,
            resolution,
        )?;
        let request_details = json!({
            "session_id": &session_id,
            "window_hwnd": window_hwnd,
            "cdp_target_id": &cdp_target_id,
            "state": wait.state,
            "text_len": wait.text.as_deref().map(str::len),
            "timeout_ms": wait.timeout_ms,
            "polling_interval_ms": wait.polling_interval_ms,
            "required_foreground": false,
        });
        self.audit_action_started_with_details_for_session(TOOL, &request_details, &session_id)?;
        let result = self
            .browser_wait_for_impl(&session_id, window_hwnd, &cdp_target_id, &wait)
            .await;
        self.audit_action_result_for_session(TOOL, &result, &session_id)?;
        result.map(Json)
    }

    /// Page-lifecycle wait predicate — internal lane for `browser_wait_for` (#1348).
    pub async fn browser_wait_for_load_state_inner(
        &self,
        params: Parameters<BrowserWaitForLoadStateParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<BrowserWaitForLoadStateResponse>, ErrorData> {
        const TOOL: &str = "browser_wait_for_load_state";
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = TOOL,
            "tool.invocation kind=browser_wait_for_load_state"
        );
        let session_id = require_target_session_id(&request_context)?;
        let wait = validate_browser_wait_for_load_state_params(&params.0)?;
        let request_details = json!({
            "session_id": &session_id,
            "window_hwnd": params.0.window_hwnd,
            "requested_cdp_target": cdp_target_id_audit_ref(params.0.cdp_target_id.as_deref()),
            "state": wait.state,
            "timeout_ms": wait.timeout_ms,
            "required_foreground": false,
            "phase": "target_resolution",
        });
        let resolution = self.resolve_cdp_tab_mutation_target(
            TOOL,
            &session_id,
            params.0.window_hwnd,
            params.0.cdp_target_id.as_deref(),
        );
        let (window_hwnd, cdp_target_id) = self.audit_cdp_target_resolution_result(
            TOOL,
            &session_id,
            &request_details,
            resolution,
        )?;
        let request_details = json!({
            "session_id": &session_id,
            "window_hwnd": window_hwnd,
            "cdp_target_id": &cdp_target_id,
            "state": wait.state,
            "timeout_ms": wait.timeout_ms,
            "required_foreground": false,
        });
        self.audit_action_started_with_details_for_session(TOOL, &request_details, &session_id)?;
        let result = self
            .browser_wait_for_load_state_impl(&session_id, window_hwnd, &cdp_target_id, &wait)
            .await;
        self.audit_action_result_for_session(TOOL, &result, &session_id)?;
        result.map(Json)
    }

    /// URL-match wait predicate — internal lane for `browser_wait_for` (#1348).
    pub async fn browser_wait_for_url_inner(
        &self,
        params: Parameters<BrowserWaitForUrlParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<BrowserWaitForUrlResponse>, ErrorData> {
        const TOOL: &str = "browser_wait_for_url";
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = TOOL,
            "tool.invocation kind=browser_wait_for_url"
        );
        let session_id = require_target_session_id(&request_context)?;
        let wait = validate_browser_wait_for_url_params(&params.0)?;
        let request_details = json!({
            "session_id": &session_id,
            "window_hwnd": params.0.window_hwnd,
            "requested_cdp_target": cdp_target_id_audit_ref(params.0.cdp_target_id.as_deref()),
            "url_len": wait.url.len(),
            "match_kind": wait.match_kind,
            "timeout_ms": wait.timeout_ms,
            "polling_interval_ms": wait.polling_interval_ms,
            "required_foreground": false,
            "phase": "target_resolution",
        });
        let resolution = self.resolve_cdp_tab_mutation_target(
            TOOL,
            &session_id,
            params.0.window_hwnd,
            params.0.cdp_target_id.as_deref(),
        );
        let (window_hwnd, cdp_target_id) = self.audit_cdp_target_resolution_result(
            TOOL,
            &session_id,
            &request_details,
            resolution,
        )?;
        let request_details = json!({
            "session_id": &session_id,
            "window_hwnd": window_hwnd,
            "cdp_target_id": &cdp_target_id,
            "url_len": wait.url.len(),
            "match_kind": wait.match_kind,
            "timeout_ms": wait.timeout_ms,
            "polling_interval_ms": wait.polling_interval_ms,
            "required_foreground": false,
        });
        self.audit_action_started_with_details_for_session(TOOL, &request_details, &session_id)?;
        let result = self
            .browser_wait_for_url_impl(&session_id, window_hwnd, &cdp_target_id, &wait)
            .await;
        self.audit_action_result_for_session(TOOL, &result, &session_id)?;
        result.map(Json)
    }

    /// Network-request wait predicate — internal lane for `browser_wait_for` (#1348).
    pub async fn browser_wait_for_request_inner(
        &self,
        params: Parameters<BrowserWaitForRequestParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<BrowserWaitForRequestResponse>, ErrorData> {
        const TOOL: &str = "browser_wait_for_request";
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = TOOL,
            "tool.invocation kind=browser_wait_for_request"
        );
        let session_id = require_target_session_id(&request_context)?;
        let wait = validate_browser_wait_for_request_params(&params.0)?;
        let request_details = json!({
            "session_id": &session_id,
            "window_hwnd": params.0.window_hwnd,
            "requested_cdp_target": cdp_target_id_audit_ref(params.0.cdp_target_id.as_deref()),
            "url_len": wait.url.as_deref().map(str::len),
            "match_kind": wait.match_kind,
            "method": wait.method.as_deref(),
            "resource_type": wait.resource_type.as_deref(),
            "timeout_ms": wait.timeout_ms,
            "polling_interval_ms": wait.polling_interval_ms,
            "required_foreground": false,
            "phase": "target_resolution",
        });
        let resolution = self.resolve_cdp_tab_mutation_target(
            TOOL,
            &session_id,
            params.0.window_hwnd,
            params.0.cdp_target_id.as_deref(),
        );
        let (window_hwnd, cdp_target_id) = self.audit_cdp_target_resolution_result(
            TOOL,
            &session_id,
            &request_details,
            resolution,
        )?;
        let request_details = json!({
            "session_id": &session_id,
            "window_hwnd": window_hwnd,
            "cdp_target_id": &cdp_target_id,
            "url_len": wait.url.as_deref().map(str::len),
            "match_kind": wait.match_kind,
            "method": wait.method.as_deref(),
            "resource_type": wait.resource_type.as_deref(),
            "timeout_ms": wait.timeout_ms,
            "polling_interval_ms": wait.polling_interval_ms,
            "required_foreground": false,
        });
        self.audit_action_started_with_details_for_session(TOOL, &request_details, &session_id)?;
        let result = self
            .browser_wait_for_request_impl(&session_id, window_hwnd, &cdp_target_id, &wait)
            .await;
        self.audit_action_result_for_session(TOOL, &result, &session_id)?;
        result.map(Json)
    }

    /// Network-response wait predicate — internal lane for `browser_wait_for` (#1348).
    pub async fn browser_wait_for_response_inner(
        &self,
        params: Parameters<BrowserWaitForNetworkResponseParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<BrowserWaitForNetworkResponseResponse>, ErrorData> {
        const TOOL: &str = "browser_wait_for_response";
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = TOOL,
            "tool.invocation kind=browser_wait_for_response"
        );
        let session_id = require_target_session_id(&request_context)?;
        let wait = validate_browser_wait_for_response_params(&params.0)?;
        let request_details = json!({
            "session_id": &session_id,
            "window_hwnd": params.0.window_hwnd,
            "requested_cdp_target": cdp_target_id_audit_ref(params.0.cdp_target_id.as_deref()),
            "url_len": wait.url.as_deref().map(str::len),
            "match_kind": wait.match_kind,
            "method": wait.method.as_deref(),
            "status": wait.status,
            "resource_type": wait.resource_type.as_deref(),
            "timeout_ms": wait.timeout_ms,
            "polling_interval_ms": wait.polling_interval_ms,
            "required_foreground": false,
            "phase": "target_resolution",
        });
        let resolution = self.resolve_cdp_tab_mutation_target(
            TOOL,
            &session_id,
            params.0.window_hwnd,
            params.0.cdp_target_id.as_deref(),
        );
        let (window_hwnd, cdp_target_id) = self.audit_cdp_target_resolution_result(
            TOOL,
            &session_id,
            &request_details,
            resolution,
        )?;
        let request_details = json!({
            "session_id": &session_id,
            "window_hwnd": window_hwnd,
            "cdp_target_id": &cdp_target_id,
            "url_len": wait.url.as_deref().map(str::len),
            "match_kind": wait.match_kind,
            "method": wait.method.as_deref(),
            "status": wait.status,
            "resource_type": wait.resource_type.as_deref(),
            "timeout_ms": wait.timeout_ms,
            "polling_interval_ms": wait.polling_interval_ms,
            "required_foreground": false,
        });
        self.audit_action_started_with_details_for_session(TOOL, &request_details, &session_id)?;
        let result = self
            .browser_wait_for_response_impl(&session_id, window_hwnd, &cdp_target_id, &wait)
            .await;
        self.audit_action_result_for_session(TOOL, &result, &session_id)?;
        result.map(Json)
    }

    /// Selector-state wait predicate — internal lane for `browser_wait_for` (#1348).
    pub async fn browser_wait_for_selector_inner(
        &self,
        params: Parameters<BrowserWaitForSelectorParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<BrowserWaitForSelectorResponse>, ErrorData> {
        const TOOL: &str = "browser_wait_for_selector";
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = TOOL,
            "tool.invocation kind=browser_wait_for_selector"
        );
        let session_id = require_target_session_id(&request_context)?;
        let wait = validate_browser_wait_for_selector_params(&params.0)?;
        let root_element = wait
            .locate
            .root_element_id
            .as_deref()
            .filter(|id| !id.trim().is_empty())
            .map(parse_browser_evaluate_element)
            .transpose()?;
        if let (Some((_, root_target)), Some(explicit)) =
            (root_element.as_ref(), wait.locate.cdp_target_id.as_deref())
            && !root_target.eq_ignore_ascii_case(explicit)
        {
            return Err(mcp_error(
                error_codes::ACTION_TARGET_INVALID,
                format!(
                    "browser_wait_for_selector root_element_id resolves to CDP target {root_target:?} but cdp_target_id {explicit:?} was also supplied; they must match"
                ),
            ));
        }
        let frame_element_target = wait
            .locate
            .frame
            .as_ref()
            .and_then(|frame| frame.frame_element_id.as_deref())
            .filter(|id| !id.trim().is_empty())
            .map(parse_browser_evaluate_element)
            .transpose()?
            .map(|(_, target)| target);
        if let (Some((_, root_target)), Some(frame_target)) =
            (root_element.as_ref(), frame_element_target.as_ref())
            && !root_target.eq_ignore_ascii_case(frame_target)
        {
            return Err(mcp_error(
                error_codes::ACTION_TARGET_INVALID,
                format!(
                    "browser_wait_for_selector root_element_id resolves to CDP target {root_target:?} but frame.frame_element_id resolves to CDP target {frame_target:?}; they must match"
                ),
            ));
        }
        if let (Some(frame_target), Some(explicit)) = (
            frame_element_target.as_ref(),
            wait.locate.cdp_target_id.as_deref(),
        ) && !frame_target.eq_ignore_ascii_case(explicit)
        {
            return Err(mcp_error(
                error_codes::ACTION_TARGET_INVALID,
                format!(
                    "browser_wait_for_selector frame.frame_element_id resolves to CDP target {frame_target:?} but cdp_target_id {explicit:?} was also supplied; they must match"
                ),
            ));
        }
        let resolution_target = wait
            .locate
            .cdp_target_id
            .clone()
            .or_else(|| root_element.as_ref().map(|(_, target)| target.clone()))
            .or(frame_element_target);
        let root_backend_node_id = root_element.as_ref().map(|(backend, _)| *backend);
        let request_details = json!({
            "session_id": &session_id,
            "window_hwnd": wait.locate.window_hwnd,
            "requested_cdp_target": cdp_target_id_audit_ref(resolution_target.as_deref()),
            "engine": wait.locate.engine,
            "query_len": wait.locate.query.len(),
            "state": wait.state,
            "root_element_id": wait.locate.root_element_id,
            "frame": wait.locate.frame,
            "limit": wait.limit,
            "timeout_ms": wait.timeout_ms,
            "polling_interval_ms": wait.polling_interval_ms,
            "required_foreground": false,
            "phase": "target_resolution",
        });
        let resolution = self.resolve_cdp_tab_mutation_target(
            TOOL,
            &session_id,
            wait.locate.window_hwnd,
            resolution_target.as_deref(),
        );
        let (window_hwnd, cdp_target_id) = self.audit_cdp_target_resolution_result(
            TOOL,
            &session_id,
            &request_details,
            resolution,
        )?;
        let request_details = json!({
            "session_id": &session_id,
            "window_hwnd": window_hwnd,
            "cdp_target_id": &cdp_target_id,
            "engine": wait.locate.engine,
            "query_len": wait.locate.query.len(),
            "state": wait.state,
            "root_element_id": wait.locate.root_element_id,
            "frame": wait.locate.frame,
            "nth": wait.locate.nth,
            "strict": wait.locate.strict,
            "limit": wait.limit,
            "timeout_ms": wait.timeout_ms,
            "polling_interval_ms": wait.polling_interval_ms,
            "required_foreground": false,
        });
        self.audit_action_started_with_details_for_session(TOOL, &request_details, &session_id)?;
        let result = self
            .browser_wait_for_selector_impl(
                &session_id,
                window_hwnd,
                &cdp_target_id,
                &wait,
                root_backend_node_id,
            )
            .await;
        self.audit_action_result_for_session(TOOL, &result, &session_id)?;
        result.map(Json)
    }

    /// JavaScript-predicate wait — internal lane for `browser_wait_for` (#1348).
    pub async fn browser_wait_for_function_inner(
        &self,
        params: Parameters<BrowserWaitForFunctionParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<BrowserWaitForFunctionResponse>, ErrorData> {
        const TOOL: &str = "browser_wait_for_function";
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = TOOL,
            "tool.invocation kind=browser_wait_for_function"
        );
        let session_id = require_target_session_id(&request_context)?;
        let wait = validate_browser_wait_for_function_params(&params.0)?;
        let request_details = json!({
            "session_id": &session_id,
            "window_hwnd": params.0.window_hwnd,
            "requested_cdp_target": cdp_target_id_audit_ref(params.0.cdp_target_id.as_deref()),
            "expression_len": wait.expression.len(),
            "arg_count": wait.args.len(),
            "timeout_ms": wait.timeout_ms,
            "polling_interval_ms": wait.polling_interval_ms,
            "required_foreground": false,
            "phase": "target_resolution",
        });
        let resolution = self.resolve_cdp_tab_mutation_target(
            TOOL,
            &session_id,
            params.0.window_hwnd,
            params.0.cdp_target_id.as_deref(),
        );
        let (window_hwnd, cdp_target_id) = self.audit_cdp_target_resolution_result(
            TOOL,
            &session_id,
            &request_details,
            resolution,
        )?;
        let request_details = json!({
            "session_id": &session_id,
            "window_hwnd": window_hwnd,
            "cdp_target_id": &cdp_target_id,
            "expression_len": wait.expression.len(),
            "arg_count": wait.args.len(),
            "timeout_ms": wait.timeout_ms,
            "polling_interval_ms": wait.polling_interval_ms,
            "required_foreground": false,
        });
        self.audit_action_started_with_details_for_session(TOOL, &request_details, &session_id)?;
        let result = self
            .browser_wait_for_function_impl(&session_id, window_hwnd, &cdp_target_id, &wait)
            .await;
        self.audit_action_result_for_session(TOOL, &result, &session_id)?;
        result.map(Json)
    }

    #[tool(
        description = "Return the full serialized HTML of the calling session's owned background browser tab (document.documentElement.outerHTML), plus url/title/readyState read back from the same target. Uses raw CDP when available or the typed normal Chrome bridge pageContent helper for chrome-tab:* targets; never the human foreground tab. Read-only, background-safe: never activates the tab or uses OS foreground input. The HTML is truncated in-page to max_bytes; html_len/truncated report the original size."
    )]
    pub async fn browser_content(
        &self,
        params: Parameters<BrowserContentParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<BrowserContentResponse>, ErrorData> {
        const TOOL: &str = "browser_content";
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = TOOL,
            "tool.invocation kind=browser_content"
        );
        let session_id = require_target_session_id(&request_context)?;
        if let Some(target_id) = params.0.cdp_target_id.as_deref() {
            validate_cdp_target_id(target_id)?;
        }
        let max_bytes = params
            .0
            .max_bytes
            .unwrap_or(DEFAULT_BROWSER_CONTENT_MAX_BYTES)
            .min(MAX_BROWSER_CONTENT_BYTES);
        let request_details = json!({
            "session_id": &session_id,
            "window_hwnd": params.0.window_hwnd,
            "requested_cdp_target": cdp_target_id_audit_ref(params.0.cdp_target_id.as_deref()),
            "max_bytes": max_bytes,
            "required_foreground": false,
            "phase": "target_resolution",
        });
        let resolution = self.resolve_cdp_tab_mutation_target(
            TOOL,
            &session_id,
            params.0.window_hwnd,
            params.0.cdp_target_id.as_deref(),
        );
        let (window_hwnd, cdp_target_id) = self.audit_cdp_target_resolution_result(
            TOOL,
            &session_id,
            &request_details,
            resolution,
        )?;
        let request_details = json!({
            "session_id": &session_id,
            "window_hwnd": window_hwnd,
            "cdp_target_id": &cdp_target_id,
            "max_bytes": max_bytes,
            "required_foreground": false,
        });
        self.audit_action_started_with_details_for_session(TOOL, &request_details, &session_id)?;
        let result = self
            .browser_content_impl(&session_id, window_hwnd, &cdp_target_id, max_bytes)
            .await;
        self.audit_action_result_for_session(TOOL, &result, &session_id)?;
        result.map(Json)
    }

    #[tool(
        description = "Replace the full document HTML of the calling session's owned background browser tab, then read back URL/title/readyState/history from the same target. Uses raw CDP Page.setDocumentContent when available or the typed normal Chrome bridge setContent helper for chrome-tab:* targets; the normal bridge seeds inaccessible blank/internal pages on the daemon-local origin before replacement. Never activates the tab, never uses OS foreground input, and never falls back to the human foreground tab."
    )]
    pub async fn browser_set_content(
        &self,
        params: Parameters<BrowserSetContentParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<BrowserSetContentResponse>, ErrorData> {
        const TOOL: &str = "browser_set_content";
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = TOOL,
            "tool.invocation kind=browser_set_content"
        );
        let session_id = require_target_session_id(&request_context)?;
        validate_browser_set_content_params(&params.0)?;
        let wait_timeout_ms = validate_cdp_navigation_wait_timeout(params.0.wait_timeout_ms)?;
        let request_details = json!({
            "session_id": &session_id,
            "window_hwnd": params.0.window_hwnd,
            "requested_cdp_target": cdp_target_id_audit_ref(params.0.cdp_target_id.as_deref()),
            "html_len": params.0.html.len(),
            "wait_timeout_ms": wait_timeout_ms,
            "required_foreground": false,
            "phase": "target_resolution",
        });
        let resolution = self.resolve_cdp_tab_mutation_target(
            TOOL,
            &session_id,
            params.0.window_hwnd,
            params.0.cdp_target_id.as_deref(),
        );
        let (window_hwnd, cdp_target_id) = self.audit_cdp_target_resolution_result(
            TOOL,
            &session_id,
            &request_details,
            resolution,
        )?;
        let request_details = json!({
            "session_id": &session_id,
            "window_hwnd": window_hwnd,
            "cdp_target_id": &cdp_target_id,
            "html_len": params.0.html.len(),
            "wait_timeout_ms": wait_timeout_ms,
            "required_foreground": false,
        });
        self.audit_action_started_with_details_for_session(TOOL, &request_details, &session_id)?;
        let result = self
            .browser_set_content_impl(
                &session_id,
                window_hwnd,
                &cdp_target_id,
                &params.0.html,
                wait_timeout_ms,
            )
            .await;
        self.audit_action_result_for_session(TOOL, &result, &session_id)?;
        result.map(Json)
    }

    #[tool(
        description = "List console output and uncaught page errors captured from the calling session's owned background browser tab via raw CDP (the Playwright page.on('console')/page.on('pageerror') analogue). A persistent per-target listener is armed on first call: it enables the CDP Runtime + Log domains and buffers Runtime.consoleAPICalled (console.log/info/warn/error/debug/trace), Runtime.exceptionThrown (uncaught throws AND unhandled promise rejections, recorded distinctly via the `source` field), and Log.entryAdded (browser-internal network/security/deprecation logs) into a bounded ring buffer. Object/array arguments are reconstructed from their CDP preview into structured JSON (never [object Object]); stacks and source url:line:col are preserved. Because capture starts at arm time and Chrome does not replay console history, a target only captures messages emitted after its first call (or after it was opened with cdp_open_tab, which arms it eagerly). Filter by level/source/text_contains; poll incrementally with since_seq (pass back next_cursor). Requires an active session CDP target or an explicit cdp_target_id owned by this session; never the human foreground tab. Read-only, background-safe: never activates the tab or uses OS foreground input. Raw CDP only; the debugger-free normal Chrome bridge fails closed because it never attaches the debugger."
    )]
    pub async fn browser_console_messages(
        &self,
        params: Parameters<BrowserConsoleMessagesParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<BrowserConsoleMessagesResponse>, ErrorData> {
        const TOOL: &str = "browser_console_messages";
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = TOOL,
            "tool.invocation kind=browser_console_messages"
        );
        let session_id = require_target_session_id(&request_context)?;
        if let Some(target_id) = params.0.cdp_target_id.as_deref() {
            validate_cdp_target_id(target_id)?;
        }
        let max_messages = params
            .0
            .max_messages
            .unwrap_or(DEFAULT_BROWSER_CONSOLE_MESSAGES)
            .min(MAX_BROWSER_CONSOLE_MESSAGES);
        let request_details = json!({
            "session_id": &session_id,
            "window_hwnd": params.0.window_hwnd,
            "requested_cdp_target": cdp_target_id_audit_ref(params.0.cdp_target_id.as_deref()),
            "since_seq": params.0.since_seq,
            "level": params.0.level,
            "source": params.0.source,
            "max_messages": max_messages,
            "required_foreground": false,
            "phase": "target_resolution",
        });
        let resolution = self.resolve_cdp_tab_mutation_target(
            TOOL,
            &session_id,
            params.0.window_hwnd,
            params.0.cdp_target_id.as_deref(),
        );
        let (window_hwnd, cdp_target_id) = self.audit_cdp_target_resolution_result(
            TOOL,
            &session_id,
            &request_details,
            resolution,
        )?;
        let request_details = json!({
            "session_id": &session_id,
            "window_hwnd": window_hwnd,
            "cdp_target_id": &cdp_target_id,
            "since_seq": params.0.since_seq,
            "level": params.0.level,
            "source": params.0.source,
            "max_messages": max_messages,
            "required_foreground": false,
        });
        self.audit_action_started_with_details_for_session(TOOL, &request_details, &session_id)?;
        let result = self
            .browser_console_messages_impl(
                &session_id,
                window_hwnd,
                &cdp_target_id,
                &params.0,
                max_messages,
            )
            .await;
        self.audit_action_result_for_session(TOOL, &result, &session_id)?;
        result.map(Json)
    }

    #[tool(
        description = "Typed introspection of a single DOM element in the calling session's owned background browser tab: tag_name, outer_html/inner_html/inner_text/text_content, the live attribute map, input value, boolean state queries (is_visible/is_enabled/is_checked/is_editable), page-relative bounding_box, and actionability predicates (attached, visible, stable, enabled, editable, receives_events) with structured failure reasons. Uses raw CDP for CDP backend-node element ids or the debugger-free normal Chrome bridge for chrome-tab:* DOM-path element ids. Never activates the tab, never uses OS foreground input, and never falls back to the human foreground tab. HTML/text fields are truncated to max_html_bytes."
    )]
    pub async fn browser_inspect(
        &self,
        params: Parameters<BrowserInspectParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<BrowserInspectResponse>, ErrorData> {
        const TOOL: &str = "browser_inspect";
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = TOOL,
            "tool.invocation kind=browser_inspect"
        );
        let session_id = require_target_session_id(&request_context)?;
        if params.0.element_id.trim().is_empty() {
            return Err(mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                "browser_inspect requires a non-empty element_id",
            ));
        }
        let bridge_element_target = parse_chrome_bridge_element_target(&params.0.element_id)?;
        let (backend_node_id, element_target, is_bridge_element) =
            if let Some(target) = bridge_element_target {
                (None, target, true)
            } else {
                let (backend, target) = parse_browser_evaluate_element(&params.0.element_id)?;
                (Some(backend), target, false)
            };
        if let Some(explicit) = params.0.cdp_target_id.as_deref() {
            validate_cdp_target_id(explicit)?;
            if !element_target.eq_ignore_ascii_case(explicit) {
                return Err(mcp_error(
                    error_codes::ACTION_TARGET_INVALID,
                    format!(
                        "browser_inspect element_id resolves to CDP target {element_target:?} but cdp_target_id {explicit:?} was also supplied; they must match"
                    ),
                ));
            }
        }
        let max_html_bytes = params
            .0
            .max_html_bytes
            .unwrap_or(DEFAULT_BROWSER_INSPECT_HTML_BYTES)
            .min(MAX_BROWSER_INSPECT_HTML_BYTES);
        let request_details = json!({
            "session_id": &session_id,
            "window_hwnd": params.0.window_hwnd,
            "element_id": &params.0.element_id,
            "requested_cdp_target": cdp_target_id_audit_ref(Some(element_target.as_str())),
            "max_html_bytes": max_html_bytes,
            "required_foreground": false,
            "phase": "target_resolution",
        });
        let resolution = self.resolve_cdp_tab_mutation_target(
            TOOL,
            &session_id,
            params.0.window_hwnd,
            Some(element_target.as_str()),
        );
        let (window_hwnd, cdp_target_id) = self.audit_cdp_target_resolution_result(
            TOOL,
            &session_id,
            &request_details,
            resolution,
        )?;
        let request_details = json!({
            "session_id": &session_id,
            "window_hwnd": window_hwnd,
            "cdp_target_id": &cdp_target_id,
            "element_id": &params.0.element_id,
            "max_html_bytes": max_html_bytes,
            "required_foreground": false,
        });
        self.audit_action_started_with_details_for_session(TOOL, &request_details, &session_id)?;
        let result = if is_bridge_element {
            self.browser_inspect_bridge_impl(
                &session_id,
                window_hwnd,
                &cdp_target_id,
                &params.0.element_id,
                max_html_bytes,
            )
            .await
        } else {
            self.browser_inspect_impl(
                &session_id,
                window_hwnd,
                &cdp_target_id,
                &params.0.element_id,
                backend_node_id.ok_or_else(|| {
                    mcp_error(
                        error_codes::TOOL_INTERNAL_ERROR,
                        "browser_inspect: raw CDP element id is missing its backend node id"
                            .to_string(),
                    )
                })?,
                max_html_bytes,
            )
            .await
        };
        self.audit_action_result_for_session(TOOL, &result, &session_id)?;
        result.map(Json)
    }

    #[tool(
        description = "Scroll a resolved DOM element in the calling session's owned background browser tab into view, returning before/after viewport, box-model, and nearest scroll-container readback. Uses raw CDP `DOM.scrollIntoViewIfNeeded` for CDP backend-node element ids or debugger-free normal Chrome bridge `Element.scrollIntoView` for chrome-tab:* DOM-path element ids. Handles off-screen nodes and nested scroll containers without activating the tab or using OS foreground input. The element id carries its target and must belong to this session."
    )]
    pub async fn browser_scroll_into_view(
        &self,
        params: Parameters<BrowserScrollIntoViewParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<BrowserScrollIntoViewResponse>, ErrorData> {
        const TOOL: &str = "browser_scroll_into_view";
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = TOOL,
            "tool.invocation kind=browser_scroll_into_view"
        );
        let session_id = require_target_session_id(&request_context)?;
        if params.0.element_id.trim().is_empty() {
            return Err(mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                "browser_scroll_into_view requires a non-empty element_id",
            ));
        }
        let bridge_element_target = parse_chrome_bridge_element_target(&params.0.element_id)?;
        let (backend_node_id, element_target, is_bridge_element) =
            if let Some(target) = bridge_element_target {
                (None, target, true)
            } else {
                let (backend, target) = parse_browser_evaluate_element(&params.0.element_id)?;
                (Some(backend), target, false)
            };
        if let Some(explicit) = params.0.cdp_target_id.as_deref() {
            validate_cdp_target_id(explicit)?;
            if !element_target.eq_ignore_ascii_case(explicit) {
                return Err(mcp_error(
                    error_codes::ACTION_TARGET_INVALID,
                    format!(
                        "browser_scroll_into_view element_id resolves to CDP target {element_target:?} but cdp_target_id {explicit:?} was also supplied; they must match"
                    ),
                ));
            }
        }
        let request_details = json!({
            "session_id": &session_id,
            "window_hwnd": params.0.window_hwnd,
            "element_id": &params.0.element_id,
            "requested_cdp_target": cdp_target_id_audit_ref(Some(element_target.as_str())),
            "required_foreground": false,
            "phase": "target_resolution",
        });
        let resolution = self.resolve_cdp_tab_mutation_target(
            TOOL,
            &session_id,
            params.0.window_hwnd,
            Some(element_target.as_str()),
        );
        let (window_hwnd, cdp_target_id) = self.audit_cdp_target_resolution_result(
            TOOL,
            &session_id,
            &request_details,
            resolution,
        )?;
        let request_details = json!({
            "session_id": &session_id,
            "window_hwnd": window_hwnd,
            "cdp_target_id": &cdp_target_id,
            "element_id": &params.0.element_id,
            "required_foreground": false,
        });
        self.audit_action_started_with_details_for_session(TOOL, &request_details, &session_id)?;
        let result = if is_bridge_element {
            self.browser_scroll_into_view_bridge_impl(
                &session_id,
                window_hwnd,
                &cdp_target_id,
                &params.0.element_id,
            )
            .await
        } else {
            self.browser_scroll_into_view_impl(
                &session_id,
                window_hwnd,
                &cdp_target_id,
                &params.0.element_id,
                backend_node_id.ok_or_else(|| {
                    mcp_error(
                        error_codes::TOOL_INTERNAL_ERROR,
                        "browser_scroll_into_view: raw CDP element id is missing its backend node id"
                            .to_string(),
                    )
                })?,
            )
            .await
        };
        self.audit_action_result_for_session(TOOL, &result, &session_id)?;
        result.map(Json)
    }

    #[tool(
        description = "Resolve any Playwright-style selector to element ids in the calling session's owned background browser tab. Uses raw CDP when available or the debugger-free normal Chrome bridge for chrome-tab:* targets. engine in css | xpath | text | role | label | placeholder | alttext | title | testid | layout (default css); `query` is the CSS/XPath text, visible text (getByText), ARIA role token (getByRole), label/placeholder/alt/title text, test-id value, or (layout) the base CSS. Options: exact/regex (text & attribute engines), name/name_exact/name_regex + ARIA state filters checked/pressed/expanded/selected/disabled/level/include_hidden (role), testid_attribute (testid, default data-testid), relation+anchor+max_distance (layout), has_text filter, nth (.first/.last via 0/-1, negative counts from end), strict (error on >1 unless nth), root_element_id (scope/chain within an element), frame {frame_id|frame_element_id|name|url|index}; the normal Chrome bridge supports explicit frame_id/name/url/index scoping and reports frame_element_id locators as unresolved when owner ids are unavailable. Returns match_count (Playwright count()), the resolved element_ids (capped at limit) that feed directly into browser_inspect / target_act / etc., frame readback when scoped, and url/title. Requires an active session CDP target or an explicit cdp_target_id owned by this session; never the human foreground tab. Read-only, background-safe."
    )]
    pub async fn browser_locate(
        &self,
        params: Parameters<BrowserLocateParams>,
        request_context: RequestContext<RoleServer>,
    ) -> Result<Json<BrowserLocateResponse>, ErrorData> {
        const TOOL: &str = "browser_locate";
        tracing::info!(
            code = "MCP_TOOL_INVOCATION",
            kind = TOOL,
            "tool.invocation kind=browser_locate"
        );
        let session_id = require_target_session_id(&request_context)?;
        if params.0.query.trim().is_empty() {
            return Err(mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                "browser_locate requires a non-empty query",
            ));
        }
        if params.0.query.len() > BROWSER_LOCATE_MAX_SELECTOR_BYTES {
            return Err(mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                format!(
                    "browser_locate query is {} bytes; the maximum is {BROWSER_LOCATE_MAX_SELECTOR_BYTES}",
                    params.0.query.len()
                ),
            ));
        }
        // Fail loud on contradictory matching modes rather than silently picking.
        if params.0.exact == Some(true) && params.0.regex == Some(true) {
            return Err(mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                "browser_locate exact and regex are mutually exclusive",
            ));
        }
        if params.0.name_exact == Some(true) && params.0.name_regex == Some(true) {
            return Err(mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                "browser_locate name_exact and name_regex are mutually exclusive",
            ));
        }
        if params.0.engine == BrowserLocateEngine::Layout {
            if params.0.relation.is_none() {
                return Err(mcp_error(
                    error_codes::TOOL_PARAMS_INVALID,
                    "browser_locate layout engine requires `relation` (near|right-of|left-of|above|below)",
                ));
            }
            if params
                .0
                .anchor
                .as_deref()
                .is_none_or(|anchor| anchor.trim().is_empty())
            {
                return Err(mcp_error(
                    error_codes::TOOL_PARAMS_INVALID,
                    "browser_locate layout engine requires a non-empty `anchor` CSS selector",
                ));
            }
        }
        validate_browser_frame_locator(TOOL, params.0.frame.as_ref())?;
        if let Some(target_id) = params.0.cdp_target_id.as_deref() {
            validate_cdp_target_id(target_id)?;
        }
        // A root_element_id scopes the search and carries its own CDP target,
        // which must agree with any explicit cdp_target_id.
        let root_element_id = params
            .0
            .root_element_id
            .as_deref()
            .filter(|id| !id.trim().is_empty());
        let root_bridge_target = match root_element_id {
            Some(id) => parse_chrome_bridge_element_target(id)?,
            None => None,
        };
        let root_element = if root_bridge_target.is_none() {
            root_element_id
                .map(parse_browser_evaluate_element)
                .transpose()?
        } else {
            None
        };
        if let (Some((_, root_target)), Some(explicit)) =
            (root_element.as_ref(), params.0.cdp_target_id.as_deref())
            && !root_target.eq_ignore_ascii_case(explicit)
        {
            return Err(mcp_error(
                error_codes::ACTION_TARGET_INVALID,
                format!(
                    "browser_locate root_element_id resolves to CDP target {root_target:?} but cdp_target_id {explicit:?} was also supplied; they must match"
                ),
            ));
        }
        if let (Some(root_target), Some(explicit)) = (
            root_bridge_target.as_ref(),
            params.0.cdp_target_id.as_deref(),
        ) && !root_target.eq_ignore_ascii_case(explicit)
        {
            return Err(mcp_error(
                error_codes::ACTION_TARGET_INVALID,
                format!(
                    "browser_locate root_element_id resolves to CDP target {root_target:?} but cdp_target_id {explicit:?} was also supplied; they must match"
                ),
            ));
        }
        let frame_element_id = params
            .0
            .frame
            .as_ref()
            .and_then(|frame| frame.frame_element_id.as_deref())
            .filter(|id| !id.trim().is_empty());
        let frame_bridge_target = match frame_element_id {
            Some(id) => parse_chrome_bridge_element_target(id)?,
            None => None,
        };
        let frame_element_target = if let Some(target) = frame_bridge_target {
            Some(target)
        } else {
            frame_element_id
                .map(parse_browser_evaluate_element)
                .transpose()?
                .map(|(_, target)| target)
        };
        if let (Some((_, root_target)), Some(frame_target)) =
            (root_element.as_ref(), frame_element_target.as_ref())
            && !root_target.eq_ignore_ascii_case(frame_target)
        {
            return Err(mcp_error(
                error_codes::ACTION_TARGET_INVALID,
                format!(
                    "browser_locate root_element_id resolves to CDP target {root_target:?} but frame.frame_element_id resolves to CDP target {frame_target:?}; they must match"
                ),
            ));
        }
        if let (Some(root_target), Some(frame_target)) =
            (root_bridge_target.as_ref(), frame_element_target.as_ref())
            && !root_target.eq_ignore_ascii_case(frame_target)
        {
            return Err(mcp_error(
                error_codes::ACTION_TARGET_INVALID,
                format!(
                    "browser_locate root_element_id resolves to CDP target {root_target:?} but frame.frame_element_id resolves to CDP target {frame_target:?}; they must match"
                ),
            ));
        }
        if let (Some(frame_target), Some(explicit)) = (
            frame_element_target.as_ref(),
            params.0.cdp_target_id.as_deref(),
        ) && !frame_target.eq_ignore_ascii_case(explicit)
        {
            return Err(mcp_error(
                error_codes::ACTION_TARGET_INVALID,
                format!(
                    "browser_locate frame.frame_element_id resolves to CDP target {frame_target:?} but cdp_target_id {explicit:?} was also supplied; they must match"
                ),
            ));
        }
        let resolution_target = params
            .0
            .cdp_target_id
            .clone()
            .or_else(|| root_element.as_ref().map(|(_, target)| target.clone()))
            .or(root_bridge_target)
            .or(frame_element_target);
        let root_backend_node_id = root_element.as_ref().map(|(backend, _)| *backend);
        let limit = params
            .0
            .limit
            .unwrap_or(DEFAULT_BROWSER_LOCATE_LIMIT)
            .clamp(1, MAX_BROWSER_LOCATE_LIMIT);
        let request_details = json!({
            "session_id": &session_id,
            "window_hwnd": params.0.window_hwnd,
            "requested_cdp_target": cdp_target_id_audit_ref(resolution_target.as_deref()),
            "engine": params.0.engine,
            "query_len": params.0.query.len(),
            "root_element_id": params.0.root_element_id,
            "frame": params.0.frame,
            "limit": limit,
            "required_foreground": false,
            "phase": "target_resolution",
        });
        let resolution = self.resolve_cdp_tab_mutation_target(
            TOOL,
            &session_id,
            params.0.window_hwnd,
            resolution_target.as_deref(),
        );
        let (window_hwnd, cdp_target_id) = self.audit_cdp_target_resolution_result(
            TOOL,
            &session_id,
            &request_details,
            resolution,
        )?;
        let request_details = json!({
            "session_id": &session_id,
            "window_hwnd": window_hwnd,
            "cdp_target_id": &cdp_target_id,
            "engine": params.0.engine,
            "query_len": params.0.query.len(),
            "root_element_id": params.0.root_element_id,
            "frame": params.0.frame,
            "nth": params.0.nth,
            "strict": params.0.strict,
            "limit": limit,
            "required_foreground": false,
        });
        self.audit_action_started_with_details_for_session(TOOL, &request_details, &session_id)?;
        let result = self
            .browser_locate_impl(
                &session_id,
                window_hwnd,
                &cdp_target_id,
                &params.0,
                root_backend_node_id,
                limit,
            )
            .await;
        self.audit_action_result_for_session(TOOL, &result, &session_id)?;
        result.map(Json)
    }
}

/// Resolves the calling session id for target tools, failing loud when absent
/// (the target registry is per-session).
pub(super) fn require_target_session_id(
    request_context: &RequestContext<RoleServer>,
) -> Result<String, ErrorData> {
    super::context::mcp_session_id_from_request_context(request_context)?.ok_or_else(|| {
        mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "target tools require an MCP session id (run the daemon in HTTP mode so each agent has its own Mcp-Session-Id)",
        )
    })
}

impl SynapseService {
    fn set_session_target(
        &self,
        session_id: &str,
        target: SessionTarget,
    ) -> Result<Option<TargetWire>, ErrorData> {
        let previous = self
            .memory_session_target(session_id)?
            .as_ref()
            .map(target_wire);
        self.persist_session_target(session_id, &target)?;
        let mut guard = self.lock_session_targets()?;
        guard.insert(session_id.to_owned(), target);
        Ok(previous)
    }

    fn get_session_target_wire(&self, session_id: &str) -> Result<Option<TargetWire>, ErrorData> {
        self.session_target(Some(session_id))
            .map(|target| target.as_ref().map(target_wire))
    }

    fn clear_session_target(&self, session_id: &str) -> Result<Option<TargetWire>, ErrorData> {
        let previous = self
            .memory_session_target(session_id)?
            .as_ref()
            .map(target_wire);
        self.delete_persisted_session_target(session_id)?;
        let mut guard = self.lock_session_targets()?;
        guard.remove(session_id);
        Ok(previous)
    }

    fn clear_session_cdp_target_if_matches(
        &self,
        session_id: &str,
        cdp_target_id: &str,
    ) -> Result<Option<TargetWire>, ErrorData> {
        let expected = {
            let guard = self.lock_session_targets()?;
            match guard.get(session_id) {
                Some(
                    target @ SessionTarget::Cdp {
                        cdp_target_id: current,
                        ..
                    },
                ) if current == cdp_target_id => Some(target.clone()),
                _ => None,
            }
        };
        let Some(expected) = expected else {
            return Ok(None);
        };
        self.delete_persisted_session_target_if_matches(session_id, &expected)?;
        let mut guard = self.lock_session_targets()?;
        let should_clear = matches!(
            guard.get(session_id),
            Some(SessionTarget::Cdp {
                cdp_target_id: current,
                ..
            }) if current == cdp_target_id
        );
        let previous = if should_clear {
            guard.remove(session_id).map(|prior| target_wire(&prior))
        } else {
            None
        };
        drop(guard);
        Ok(previous)
    }

    fn lock_session_targets(
        &self,
    ) -> Result<
        std::sync::MutexGuard<'_, std::collections::HashMap<String, SessionTarget>>,
        ErrorData,
    > {
        self.session_targets_ref().lock().map_err(|_err| {
            mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                "session target registry lock poisoned",
            )
        })
    }

    fn lock_cdp_target_owners(
        &self,
    ) -> Result<
        std::sync::MutexGuard<'_, std::collections::HashMap<String, CdpTargetOwner>>,
        ErrorData,
    > {
        self.cdp_target_owners_ref().lock().map_err(|_err| {
            mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                "CDP target ownership registry lock poisoned",
            )
        })
    }

    fn request_session_target(
        &self,
        request_context: &RequestContext<RoleServer>,
    ) -> Result<Option<SessionTarget>, ErrorData> {
        let session_id = super::context::mcp_session_id_from_request_context(request_context)?;
        let target = self.session_target(session_id.as_deref())?;
        if let (Some(session_id), Some(target)) = (session_id.as_deref(), target.as_ref()) {
            match target {
                SessionTarget::Window { hwnd } => tracing::debug!(
                    code = "SESSION_TARGET_RESOLVED",
                    session_id = %session_id,
                    hwnd,
                    "readback=session_target outcome=resolved_window"
                ),
                SessionTarget::Cdp {
                    window_hwnd,
                    cdp_target_id,
                } => tracing::debug!(
                    code = "SESSION_TARGET_RESOLVED",
                    session_id = %session_id,
                    hwnd = *window_hwnd,
                    cdp_target_id = %cdp_target_id,
                    "readback=session_target outcome=resolved_cdp"
                ),
            }
        }
        Ok(target)
    }

    fn resolve_cdp_context_window(
        &self,
        session_id: &str,
        explicit_window_hwnd: Option<i64>,
    ) -> Result<i64, ErrorData> {
        if let Some(window_hwnd) = explicit_window_hwnd {
            return Ok(window_hwnd);
        }
        match self.session_target(Some(session_id))? {
            Some(SessionTarget::Window { hwnd }) => Ok(hwnd),
            Some(SessionTarget::Cdp { window_hwnd, .. }) => Ok(window_hwnd),
            None => Err(mcp_error(
                error_codes::TARGET_NOT_SET,
                "cdp_open_tab requires window_hwnd or an existing session target; refusing to use the human foreground as an implicit browser",
            )),
        }
    }

    pub(super) fn register_cdp_target_owner(
        &self,
        owner: CdpTargetOwner,
    ) -> Result<String, ErrorData> {
        let owner_key =
            cdp_target_owner_key(owner.window_hwnd, &owner.endpoint, &owner.cdp_target_id);
        {
            let guard = self.lock_cdp_target_owners()?;
            if let Some(existing) = guard.get(&owner_key) {
                return Err(mcp_error(
                    error_codes::ACTION_TARGET_INVALID,
                    format!(
                        "CDP target {:?} on endpoint {:?} window {:#x} is already owned by MCP session {:?}",
                        existing.cdp_target_id,
                        existing.endpoint,
                        existing.window_hwnd,
                        existing.session_id
                    ),
                ));
            }
        }
        self.persist_cdp_target_owner(&owner_key, &owner)?;
        let mut guard = self.lock_cdp_target_owners()?;
        if let Some(existing) = guard.get(&owner_key) {
            self.delete_persisted_cdp_target_owner(&owner_key, &owner.cdp_target_id)?;
            return Err(mcp_error(
                error_codes::ACTION_TARGET_INVALID,
                format!(
                    "CDP target {:?} on endpoint {:?} window {:#x} became owned by MCP session {:?} during registration",
                    existing.cdp_target_id,
                    existing.endpoint,
                    existing.window_hwnd,
                    existing.session_id
                ),
            ));
        }
        guard.insert(owner_key.clone(), owner);
        drop(guard);
        Ok(owner_key)
    }

    fn remove_cdp_target_owner(
        &self,
        owner_key: &str,
    ) -> Result<Option<CdpTargetOwner>, ErrorData> {
        let mut guard = self.lock_cdp_target_owners()?;
        let removed = guard.remove(owner_key);
        drop(guard);
        if let Some(owner) = removed.as_ref() {
            self.delete_persisted_cdp_target_owner(owner_key, &owner.cdp_target_id)?;
        }
        Ok(removed)
    }

    fn release_closed_cdp_target_claim(
        &self,
        session_id: &str,
        window_hwnd: i64,
        cdp_target_id: &str,
    ) -> Result<bool, ErrorData> {
        let target = SessionTarget::Cdp {
            window_hwnd,
            cdp_target_id: cdp_target_id.to_owned(),
        };
        let target_key = super::target_claims::target_key(&target);
        let mut guard = self.lock_target_claims()?;
        match guard.release(session_id, &target_key) {
            Ok(released) => Ok(released.is_some()),
            Err(conflict) => {
                tracing::warn!(
                    code = "CDP_BACKGROUND_TAB_CLAIM_RELEASE_CONFLICT",
                    session_id = %session_id,
                    target_key = %target_key,
                    conflict = ?conflict,
                    "readback=target_claim_registry outcome=claim_left_visible"
                );
                Ok(false)
            }
        }
    }

    fn cdp_target_owner_for_close(
        &self,
        session_id: &str,
        target_id: &str,
        explicit_target_authority: Option<&SessionTarget>,
    ) -> Result<(String, CdpTargetOwner), ErrorData> {
        let active_target = self.session_target(Some(session_id))?;
        let owners = self.cdp_target_owners_for_target_id(target_id)?;
        let owned_by_session = owners
            .iter()
            .filter(|(_key, owner)| owner.session_id == session_id)
            .cloned()
            .collect::<Vec<_>>();
        if !owned_by_session.is_empty() {
            return select_cdp_owner_for_session(
                "cdp_close_tab",
                session_id,
                target_id,
                active_target.as_ref(),
                owned_by_session,
            );
        }
        match self.recover_cdp_target_owner_for_authorized_session(
            "cdp_close_tab",
            session_id,
            target_id,
            active_target.as_ref(),
            explicit_target_authority,
        )? {
            Some(recovered) => Ok(recovered),
            None => Err(cdp_close_unowned_error(target_id, session_id, &owners)),
        }
    }

    fn recover_cdp_target_owner_for_authorized_session(
        &self,
        tool: &str,
        session_id: &str,
        target_id: &str,
        active_target: Option<&SessionTarget>,
        explicit_target_authority: Option<&SessionTarget>,
    ) -> Result<Option<(String, CdpTargetOwner)>, ErrorData> {
        let persisted = self.read_persisted_cdp_target_owners_for_target_id(target_id)?;
        if persisted.is_empty() {
            return Ok(None);
        }
        let mut authorized = Vec::new();
        for (owner_key, row) in persisted {
            let target = SessionTarget::Cdp {
                window_hwnd: row.owner.window_hwnd,
                cdp_target_id: row.owner.cdp_target_id.clone(),
            };
            if self
                .target_claim_for_session(session_id, &target)?
                .is_some()
                || active_target.is_some_and(|active| session_targets_equal(active, &target))
                || explicit_target_authority
                    .is_some_and(|explicit| session_targets_equal(explicit, &target))
            {
                authorized.push((owner_key, row));
            }
        }
        if authorized.is_empty() {
            return Err(mcp_error(
                error_codes::ACTION_TARGET_INVALID,
                format!(
                    "{tool} refused recovered target {target_id:?}: this session must hold an exact target_claim, exact active CDP session target, or exact explicit set_target request for the persisted CDP target before durable owner authority can be restored"
                ),
            ));
        }
        let selected = select_persisted_cdp_owner_for_authorized_session(
            tool,
            session_id,
            target_id,
            active_target,
            explicit_target_authority,
            authorized,
        )?;
        self.ensure_persisted_cdp_owner_recoverable(tool, session_id, &selected.1)?;
        let mut owner = selected.1.owner.clone();
        owner.session_id = session_id.to_owned();
        let owner_key = selected.0;
        self.persist_cdp_target_owner(&owner_key, &owner)?;
        {
            let mut guard = self.lock_cdp_target_owners()?;
            guard.insert(owner_key.clone(), owner.clone());
        }
        tracing::info!(
            code = "CDP_TARGET_OWNER_RECOVERED_FOR_AUTHORIZED_SESSION",
            tool = %tool,
            session_id = %session_id,
            prior_owner_session_id = %selected.1.owner_session_id,
            owner_key = %owner_key,
            hwnd = owner.window_hwnd,
            endpoint = %owner.endpoint,
            cdp_target_id = %owner.cdp_target_id,
            "readback=CF_SESSIONS+target_claim outcome=owner_authority_recovered"
        );
        Ok(Some((owner_key, owner)))
    }

    fn ensure_persisted_cdp_owner_recoverable(
        &self,
        tool: &str,
        session_id: &str,
        persisted: &PersistedCdpTargetOwner,
    ) -> Result<(), ErrorData> {
        if persisted.owner_session_id == session_id {
            return Ok(());
        }
        let now = unix_ms_now();
        let persisted_owner_registry_read =
            self.session_registry_read_optional(&persisted.owner_session_id)?;
        if let Some(owner_read) = persisted_owner_registry_read.as_ref()
            && owner_read.lifecycle == "live"
            && owner_read.started_at_unix_ms <= persisted.stored_at_unix_ms
        {
            let (_requester, owner, _in_flight) = self.ensure_same_agent_adoption_allowed(
                session_id,
                &persisted.owner_session_id,
                now,
            )?;
            tracing::info!(
                code = "CDP_TARGET_OWNER_LIVE_SAME_AGENT_RECOVERY_ALLOWED",
                session_id = %session_id,
                owner_session_id = %persisted.owner_session_id,
                owner_lifecycle = %owner.lifecycle,
                "readback=session_registry edge=live_same_agent_owner"
            );
            return Ok(());
        }
        if let Some(owner_read) = persisted_owner_registry_read.as_ref()
            && owner_read.lifecycle == "live"
        {
            tracing::info!(
                code = "CDP_TARGET_OWNER_REHYDRATED_SESSION_IGNORED_FOR_CLOSE",
                session_id = %session_id,
                owner_session_id = %persisted.owner_session_id,
                owner_started_at_unix_ms = owner_read.started_at_unix_ms,
                persisted_stored_at_unix_ms = persisted.stored_at_unix_ms,
                persisted_owner_started_at_unix_ms = ?persisted.owner_started_at_unix_ms,
                "readback=session_registry+CF_SESSIONS edge=post_restart_session_id_rehydrated"
            );
        }
        let requester = self.current_session_registry_read(tool, session_id)?;
        if requester.lifecycle != "live" {
            return Err(mcp_error(
                error_codes::ACTION_TARGET_INVALID,
                format!(
                    "{tool} refused recovered target {:?}: requesting session {session_id:?} is not live in session registry",
                    persisted.owner.cdp_target_id
                ),
            ));
        }
        if self.dead_spawned_child_owner_recovery_allowed(
            tool,
            &requester,
            persisted_owner_registry_read.as_ref(),
            persisted,
        )? {
            return Ok(());
        }
        if requester.agent_kind == "unknown"
            || persisted.owner_agent_kind == "unknown"
            || requester.agent_kind != persisted.owner_agent_kind
        {
            return Err(mcp_error(
                error_codes::ACTION_TARGET_INVALID,
                format!(
                    "{tool} refused recovered target {:?}: persisted owner agent_kind {:?} does not match requesting agent_kind {:?}",
                    persisted.owner.cdp_target_id, persisted.owner_agent_kind, requester.agent_kind
                ),
            ));
        }
        if requester.client_name.is_none() || requester.client_name != persisted.owner_client_name {
            return Err(mcp_error(
                error_codes::ACTION_TARGET_INVALID,
                format!(
                    "{tool} refused recovered target {:?}: persisted owner client_name does not match requesting session",
                    persisted.owner.cdp_target_id
                ),
            ));
        }
        if let Some(owner_started_at) = persisted.owner_started_at_unix_ms
            && requester.started_at_unix_ms <= owner_started_at
        {
            return Err(mcp_error(
                error_codes::ACTION_TARGET_INVALID,
                format!(
                    "{tool} refused recovered target {:?}: requesting session is not newer than persisted owner session",
                    persisted.owner.cdp_target_id
                ),
            ));
        }
        Ok(())
    }

    fn dead_spawned_child_owner_recovery_allowed(
        &self,
        tool: &str,
        requester: &super::session_registry::SessionRegistryRead,
        owner_read: Option<&super::session_registry::SessionRegistryRead>,
        persisted: &PersistedCdpTargetOwner,
    ) -> Result<bool, ErrorData> {
        let Some(owner_read) = owner_read else {
            return Ok(false);
        };
        let Some(spawned) = owner_read.spawned_agent.as_ref() else {
            return Ok(false);
        };
        if spawned.started_by_session_id.as_deref() != Some(requester.session_id.as_str()) {
            return Ok(false);
        }

        let mut probe_pids = Vec::new();
        if let Some(agent_pid) = spawned.agent_process_id
            && agent_pid != 0
        {
            probe_pids.push(agent_pid);
        }
        if spawned.launcher_process_id != 0 && !probe_pids.contains(&spawned.launcher_process_id) {
            probe_pids.push(spawned.launcher_process_id);
        }
        if probe_pids.is_empty() {
            return Err(mcp_error(
                error_codes::ACTION_TARGET_INVALID,
                format!(
                    "{tool} refused recovered target {:?}: spawned child owner has no recorded process ids for lineage cleanup",
                    persisted.owner.cdp_target_id
                ),
            ));
        }
        let live_pids = probe_pids
            .iter()
            .copied()
            .filter(|pid| crate::m4::process_exists(*pid))
            .collect::<Vec<_>>();
        if !live_pids.is_empty() {
            return Err(mcp_error(
                error_codes::ACTION_TARGET_INVALID,
                format!(
                    "{tool} refused recovered target {:?}: spawned child owner is still live; live_process_ids={live_pids:?}",
                    persisted.owner.cdp_target_id
                ),
            ));
        }
        let owner_in_flight = match crate::daemon_lifecycle::in_flight_tool_calls_for_session(
            &persisted.owner_session_id,
        ) {
            Ok(calls) => calls,
            #[cfg(test)]
            Err(error) if error.to_string().contains("ledger is not configured") => Vec::new(),
            Err(error) => {
                return Err(mcp_error(
                    error_codes::TOOL_INTERNAL_ERROR,
                    format!("read daemon lifecycle in-flight tool calls: {error:#}"),
                ));
            }
        };
        if !owner_in_flight.is_empty() {
            return Err(mcp_error(
                error_codes::ACTION_TARGET_INVALID,
                format!(
                    "{tool} refused recovered target {:?}: spawned child owner has in-flight tool calls",
                    persisted.owner.cdp_target_id
                ),
            ));
        }
        let lease_status = synapse_action::lease::status();
        if lease_status.owner_session_id.as_deref() == Some(persisted.owner_session_id.as_str()) {
            return Err(mcp_error(
                error_codes::ACTION_TARGET_INVALID,
                format!(
                    "{tool} refused recovered target {:?}: spawned child owner holds the foreground input lease",
                    persisted.owner.cdp_target_id
                ),
            ));
        }
        tracing::info!(
            code = "CDP_TARGET_OWNER_DEAD_SPAWNED_CHILD_RECOVERY_ALLOWED",
            session_id = %requester.session_id,
            owner_session_id = %persisted.owner_session_id,
            spawn_id = %spawned.spawn_id,
            owner_lifecycle = %owner_read.lifecycle,
            probe_pids = ?probe_pids,
            "readback=session_registry+process_table edge=dead_spawned_child_lineage"
        );
        Ok(true)
    }

    fn current_session_registry_read(
        &self,
        tool: &str,
        session_id: &str,
    ) -> Result<super::session_registry::SessionRegistryRead, ErrorData> {
        let now = unix_ms_now();
        self.session_registry_read_optional_at(session_id, now)?
            .ok_or_else(|| {
                mcp_error(
                    error_codes::ACTION_TARGET_INVALID,
                    format!(
                        "{tool} refused recovered target: requesting session {session_id:?} is missing from session registry"
                    ),
                )
            })
    }

    fn session_registry_read_optional(
        &self,
        session_id: &str,
    ) -> Result<Option<super::session_registry::SessionRegistryRead>, ErrorData> {
        self.session_registry_read_optional_at(session_id, unix_ms_now())
    }

    fn session_registry_read_optional_at(
        &self,
        session_id: &str,
        now: u64,
    ) -> Result<Option<super::session_registry::SessionRegistryRead>, ErrorData> {
        let guard = self.session_registry_ref().lock().map_err(|_error| {
            mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                "session registry lock poisoned while validating recovered CDP close authority",
            )
        })?;
        Ok(guard
            .reads(now)
            .into_iter()
            .find(|read| read.session_id == session_id))
    }

    fn resolve_cdp_navigation_target(
        &self,
        session_id: &str,
        params: &CdpNavigateTabParams,
    ) -> Result<(i64, String), ErrorData> {
        self.resolve_cdp_tab_mutation_target(
            "cdp_navigate_tab",
            session_id,
            params.window_hwnd,
            params.cdp_target_id.as_deref(),
        )
    }

    /// Resolves and ownership-checks the (window_hwnd, cdp_target_id) for a
    /// background CDP tab mutation (navigate/activate). Refuses to fall back to
    /// the human foreground tab: requires either the active CDP session target
    /// or an explicit target id owned by this MCP session. Shared by
    /// cdp_navigate_tab and cdp_activate_tab.
    pub(crate) fn resolve_cdp_tab_mutation_target(
        &self,
        tool: &str,
        session_id: &str,
        window_hwnd_param: Option<i64>,
        cdp_target_id_param: Option<&str>,
    ) -> Result<(i64, String), ErrorData> {
        if let Some(target_id) = cdp_target_id_param {
            validate_cdp_target_id(target_id)?;
        }
        let active_target = self.session_target(Some(session_id))?;
        let owner = cdp_target_id_param
            .map(|target_id| self.cdp_target_owner_for_navigation(tool, session_id, target_id))
            .transpose()?
            .flatten();
        let target_id = match (cdp_target_id_param, active_target.as_ref()) {
            (Some(target_id), _) => target_id.to_owned(),
            (None, Some(SessionTarget::Cdp { cdp_target_id, .. })) => cdp_target_id.clone(),
            (None, Some(SessionTarget::Window { .. }) | None) => {
                return Err(mcp_error(
                    error_codes::TARGET_NOT_SET,
                    format!(
                        "{tool} requires an active CDP session target or explicit cdp_target_id owned by this session; refusing to use the human foreground tab"
                    ),
                ));
            }
        };
        let window_hwnd = window_hwnd_param
            .or_else(|| owner.as_ref().map(|owner| owner.window_hwnd))
            .or_else(|| match active_target.as_ref() {
                Some(SessionTarget::Cdp { window_hwnd, .. }) => Some(*window_hwnd),
                Some(SessionTarget::Window { hwnd }) => Some(*hwnd),
                None => None,
            })
            .ok_or_else(|| {
                mcp_error(
                    error_codes::TARGET_NOT_SET,
                    format!(
                        "{tool} requires window_hwnd when using an explicit target id without an active session target"
                    ),
                )
            })?;
        let active_matches = matches!(
            active_target.as_ref(),
            Some(SessionTarget::Cdp {
                window_hwnd: active_hwnd,
                cdp_target_id: active_target_id,
            }) if *active_hwnd == window_hwnd && active_target_id.eq_ignore_ascii_case(&target_id)
        );
        if !active_matches && owner.is_none() {
            return Err(mcp_error(
                error_codes::ACTION_TARGET_INVALID,
                format!(
                    "{tool} refused target {target_id:?}: target is not the active CDP target and is not owned by this MCP session"
                ),
            ));
        }
        if let Some(owner) = owner.as_ref()
            && owner.window_hwnd != window_hwnd
        {
            return Err(mcp_error(
                error_codes::ACTION_TARGET_INVALID,
                format!(
                    "{tool} refused target {target_id:?}: owner window {:#x} does not match requested window {:#x}",
                    owner.window_hwnd, window_hwnd
                ),
            ));
        }
        Ok((window_hwnd, target_id))
    }

    fn resolve_cdp_target_info_target(
        &self,
        session_id: &str,
        params: &CdpTargetInfoParams,
    ) -> Result<(i64, String), ErrorData> {
        if let Some(target_id) = params.cdp_target_id.as_deref() {
            validate_cdp_target_id(target_id)?;
        }
        let active_target = self.session_target(Some(session_id))?;
        let owner = params
            .cdp_target_id
            .as_deref()
            .map(|target_id| {
                self.cdp_target_owner_for_readback("cdp_target_info", session_id, target_id)
            })
            .transpose()?
            .flatten();
        let target_id = match (params.cdp_target_id.as_ref(), active_target.as_ref()) {
            (Some(target_id), _) => target_id.clone(),
            (None, Some(SessionTarget::Cdp { cdp_target_id, .. })) => cdp_target_id.clone(),
            (None, Some(SessionTarget::Window { .. }) | None) => {
                return Err(mcp_error(
                    error_codes::TARGET_NOT_SET,
                    "cdp_target_info requires an active CDP session target or explicit cdp_target_id owned by this session; refusing to use the human foreground tab",
                ));
            }
        };
        let window_hwnd = params
            .window_hwnd
            .or_else(|| owner.as_ref().map(|owner| owner.window_hwnd))
            .or_else(|| match active_target.as_ref() {
                Some(SessionTarget::Cdp { window_hwnd, .. }) => Some(*window_hwnd),
                Some(SessionTarget::Window { hwnd }) => Some(*hwnd),
                None => None,
            })
            .ok_or_else(|| {
                mcp_error(
                    error_codes::TARGET_NOT_SET,
                    "cdp_target_info requires window_hwnd when using an explicit target id without an active session target",
                )
            })?;
        let active_matches = matches!(
            active_target.as_ref(),
            Some(SessionTarget::Cdp {
                window_hwnd: active_hwnd,
                cdp_target_id: active_target_id,
            }) if *active_hwnd == window_hwnd && active_target_id.eq_ignore_ascii_case(&target_id)
        );
        if !active_matches && owner.is_none() {
            return Err(mcp_error(
                error_codes::ACTION_TARGET_INVALID,
                format!(
                    "cdp_target_info refused target {target_id:?}: target is not the active CDP target and is not owned by this MCP session"
                ),
            ));
        }
        if let Some(owner) = owner.as_ref()
            && owner.window_hwnd != window_hwnd
        {
            return Err(mcp_error(
                error_codes::ACTION_TARGET_INVALID,
                format!(
                    "cdp_target_info refused target {target_id:?}: owner window {:#x} does not match requested window {:#x}",
                    owner.window_hwnd, window_hwnd
                ),
            ));
        }
        Ok((window_hwnd, target_id))
    }

    fn cdp_target_owner_for_navigation(
        &self,
        tool: &str,
        session_id: &str,
        target_id: &str,
    ) -> Result<Option<CdpTargetOwner>, ErrorData> {
        let active_target = self.session_target(Some(session_id))?;
        let owners = self.cdp_target_owners_for_target_id(target_id)?;
        let owned_by_session = owners
            .iter()
            .filter(|(_key, owner)| owner.session_id == session_id)
            .cloned()
            .collect::<Vec<_>>();
        if owned_by_session.is_empty() {
            if let Some((_key, owner)) = self.recover_cdp_target_owner_for_authorized_session(
                tool,
                session_id,
                target_id,
                active_target.as_ref(),
                None,
            )? {
                return Ok(Some(owner));
            }
            if owners.is_empty() {
                return Ok(None);
            }
            return Err(cdp_owner_session_mismatch_error(
                tool, target_id, session_id, &owners,
            ));
        }
        select_cdp_owner_for_session(
            tool,
            session_id,
            target_id,
            active_target.as_ref(),
            owned_by_session,
        )
        .map(|(_key, owner)| Some(owner))
    }

    pub(super) fn audit_cdp_target_resolution_result(
        &self,
        tool: &'static str,
        session_id: &str,
        request_details: &Value,
        result: Result<(i64, String), ErrorData>,
    ) -> Result<(i64, String), ErrorData> {
        match result {
            Ok(resolved) => Ok(resolved),
            Err(error) => {
                self.audit_action_denied_with_details_for_session(
                    tool,
                    &error,
                    request_details,
                    session_id,
                );
                Err(error)
            }
        }
    }

    pub(super) fn cdp_target_owner_for_readback(
        &self,
        tool: &str,
        session_id: &str,
        target_id: &str,
    ) -> Result<Option<CdpTargetOwner>, ErrorData> {
        let active_target = self.session_target(Some(session_id))?;
        let owners = self.cdp_target_owners_for_target_id(target_id)?;
        let owned_by_session = owners
            .iter()
            .filter(|(_key, owner)| owner.session_id == session_id)
            .cloned()
            .collect::<Vec<_>>();
        if owned_by_session.is_empty() {
            if let Some((_key, owner)) = self.recover_cdp_target_owner_for_authorized_session(
                tool,
                session_id,
                target_id,
                active_target.as_ref(),
                None,
            )? {
                return Ok(Some(owner));
            }
            if owners.is_empty() {
                return Ok(None);
            }
            return Err(cdp_owner_session_mismatch_error(
                tool, target_id, session_id, &owners,
            ));
        }
        select_cdp_owner_for_session(
            tool,
            session_id,
            target_id,
            active_target.as_ref(),
            owned_by_session,
        )
        .map(|(_key, owner)| Some(owner))
    }

    fn active_cdp_target_owner_for_window(
        &self,
        tool: &str,
        session_id: &str,
        window_hwnd: i64,
    ) -> Result<Option<CdpTargetOwner>, ErrorData> {
        let active_target = self.session_target(Some(session_id))?;
        let Some(SessionTarget::Cdp {
            window_hwnd: active_window_hwnd,
            cdp_target_id,
        }) = active_target
        else {
            return Ok(None);
        };
        if active_window_hwnd != window_hwnd {
            return Ok(None);
        }
        self.cdp_target_owner_for_readback(tool, session_id, &cdp_target_id)
    }

    fn cdp_target_owners_for_target_id(
        &self,
        target_id: &str,
    ) -> Result<Vec<(String, CdpTargetOwner)>, ErrorData> {
        let guard = self.lock_cdp_target_owners()?;
        let owners = guard
            .iter()
            .filter(|(_key, owner)| cdp_target_ids_equal(&owner.cdp_target_id, target_id))
            .map(|(key, owner)| (key.clone(), owner.clone()))
            .collect::<Vec<_>>();
        drop(guard);
        Ok(owners)
    }

    #[cfg(windows)]
    async fn ensure_cdp_target_bindable_for_window(
        &self,
        session_id: &str,
        window_hwnd: i64,
        cdp_target_id: &str,
    ) -> Result<(), ErrorData> {
        if let Some(endpoint) = synapse_a11y::endpoint_for_window(window_hwnd) {
            return self
                .ensure_cdp_target_bindable(session_id, window_hwnd, &endpoint, cdp_target_id)
                .await;
        }
        let expected_context = validate_target_window_context(window_hwnd).ok();
        crate::chrome_debugger_bridge::target_info(
            window_hwnd,
            cdp_target_id,
            None,
            expected_context
                .as_ref()
                .map(|context| context.window_bounds),
            expected_context
                .as_ref()
                .map(|context| context.window_title.as_str()),
        )
        .await
        .map_err(|error| {
            mcp_error(
                error.code(),
                format!(
                    "set_target Chrome debugger target readback failed: {}",
                    error.detail()
                ),
            )
        })?;
        {
            let guard = self.lock_cdp_target_owners()?;
            let owners = guard
                .values()
                .filter(|owner| {
                    owner.window_hwnd == window_hwnd
                        && cdp_target_ids_equal(&owner.cdp_target_id, cdp_target_id)
                })
                .cloned()
                .collect::<Vec<_>>();
            drop(guard);
            for owner in owners {
                if owner.session_id != session_id {
                    let explicit_target = SessionTarget::Cdp {
                        window_hwnd,
                        cdp_target_id: cdp_target_id.to_owned(),
                    };
                    if self
                        .recover_cdp_target_owner_for_authorized_session(
                            "set_target",
                            session_id,
                            cdp_target_id,
                            None,
                            Some(&explicit_target),
                        )?
                        .is_some()
                    {
                        continue;
                    }
                    return Err(mcp_error(
                        error_codes::ACTION_TARGET_INVALID,
                        format!(
                            "set_target refused CDP target {cdp_target_id:?}: owner_session_id={:?}, requesting_session_id={:?}",
                            owner.session_id, session_id
                        ),
                    ));
                }
                if owner.window_hwnd != window_hwnd {
                    return Err(mcp_error(
                        error_codes::ACTION_TARGET_INVALID,
                        format!(
                            "set_target refused CDP target {cdp_target_id:?}: owner registry window mismatch (owner_hwnd={:#x}, requested_hwnd={:#x})",
                            owner.window_hwnd, window_hwnd
                        ),
                    ));
                }
            }
        }
        Ok(())
    }

    #[cfg(not(windows))]
    async fn ensure_cdp_target_bindable_for_window(
        &self,
        _session_id: &str,
        _window_hwnd: i64,
        _cdp_target_id: &str,
    ) -> Result<(), ErrorData> {
        Err(mcp_error(
            error_codes::A11Y_NOT_AVAILABLE,
            "CDP target binding is only available on Windows in this build",
        ))
    }

    #[cfg(windows)]
    async fn ensure_cdp_target_bindable(
        &self,
        session_id: &str,
        window_hwnd: i64,
        endpoint: &str,
        cdp_target_id: &str,
    ) -> Result<(), ErrorData> {
        let targets = synapse_a11y::cdp_list_targets(endpoint)
            .await
            .map_err(|error| {
                mcp_error(
                    error.code(),
                    format!("set_target CDP target readback failed: {error}"),
                )
            })?;
        if !targets
            .iter()
            .any(|target| target.target_id == cdp_target_id)
        {
            return Err(mcp_error(
                error_codes::TARGET_CDP_UNRESOLVED,
                format!(
                    "set_target refused CDP target {cdp_target_id:?}: Target.getTargets readback did not contain it; available target ids: {}",
                    targets
                        .iter()
                        .map(|target| target.target_id.as_str())
                        .collect::<Vec<_>>()
                        .join(",")
                ),
            ));
        }
        let owner_key = cdp_target_owner_key(window_hwnd, endpoint, cdp_target_id);
        let existing_owner = {
            let guard = self.lock_cdp_target_owners()?;
            guard.get(&owner_key).cloned()
        };
        if let Some(owner) = existing_owner {
            if owner.window_hwnd != window_hwnd || owner.endpoint != endpoint {
                return Err(mcp_error(
                    error_codes::ACTION_TARGET_INVALID,
                    format!(
                        "set_target refused CDP target {cdp_target_id:?}: owner registry window/endpoint mismatch (owner_hwnd={:#x}, requested_hwnd={:#x})",
                        owner.window_hwnd, window_hwnd
                    ),
                ));
            }
            if owner.session_id != session_id {
                let explicit_target = SessionTarget::Cdp {
                    window_hwnd,
                    cdp_target_id: cdp_target_id.to_owned(),
                };
                if self
                    .recover_cdp_target_owner_for_authorized_session(
                        "set_target",
                        session_id,
                        cdp_target_id,
                        None,
                        Some(&explicit_target),
                    )?
                    .is_none()
                {
                    return Err(mcp_error(
                        error_codes::ACTION_TARGET_INVALID,
                        format!(
                            "set_target refused CDP target {cdp_target_id:?}: owner_session_id={:?}, requesting_session_id={:?}",
                            owner.session_id, session_id
                        ),
                    ));
                }
            }
        }
        Ok(())
    }

    #[cfg(not(windows))]
    async fn ensure_cdp_target_bindable(
        &self,
        _session_id: &str,
        _window_hwnd: i64,
        _endpoint: &str,
        _cdp_target_id: &str,
    ) -> Result<(), ErrorData> {
        Err(mcp_error(
            error_codes::A11Y_NOT_AVAILABLE,
            "CDP target binding is only available on Windows in this build",
        ))
    }

    #[cfg(windows)]
    async fn cdp_open_tab_impl(
        &self,
        session_id: &str,
        window_hwnd: i64,
        window_bounds: Rect,
        requested_url: &str,
        window_title: &str,
        process_name: &str,
    ) -> Result<CdpOpenTabResponse, ErrorData> {
        if let Some(endpoint) = synapse_a11y::endpoint_for_window(window_hwnd) {
            return self
                .cdp_open_tab_raw_impl(
                    session_id,
                    window_hwnd,
                    &endpoint,
                    requested_url,
                    window_title,
                    process_name,
                )
                .await;
        }

        let human_os_foreground_before_hwnd = current_human_os_foreground_hwnd();
        let expected_chrome_window_id = self
            .active_cdp_target_owner_for_window("cdp_open_tab", session_id, window_hwnd)?
            .and_then(|owner| owner.chrome_window_id);
        let opened = crate::chrome_debugger_bridge::open_tab(
            window_hwnd,
            requested_url,
            Some(session_id),
            expected_chrome_window_id,
            Some(window_bounds),
            Some(window_title),
        )
        .await
        .map_err(|error| {
            mcp_error(
                error.code(),
                format!(
                    "cdp_open_tab Chrome debugger chrome.tabs.create/readback failed: {}",
                    error.detail()
                ),
            )
        })?;
        let endpoint = opened
            .extension_id
            .as_deref()
            .map(chrome_debugger_endpoint)
            .unwrap_or_else(chrome_debugger_default_endpoint);
        let cdp_target_id = opened.target_id.clone();
        let chrome_window_id = opened.chrome_window_id;
        let human_os_foreground_after_hwnd = current_human_os_foreground_hwnd();
        if chrome_window_id.is_none() {
            let _ = crate::chrome_debugger_bridge::close_tab(window_hwnd, &cdp_target_id).await;
            return Err(mcp_error(
                error_codes::ACTION_POSTCONDITION_FAILED,
                format!(
                    "cdp_open_tab Chrome bridge did not return an actual chrome_window_id for target {cdp_target_id:?}"
                ),
            ));
        }
        let requested_window_was_human_foreground =
            human_os_foreground_before_hwnd == Some(window_hwnd);
        let requested_window_is_human_foreground =
            human_os_foreground_after_hwnd == Some(window_hwnd);
        if !requested_window_was_human_foreground
            && requested_window_is_human_foreground
            && human_os_foreground_after_hwnd != human_os_foreground_before_hwnd
        {
            let _ = crate::chrome_debugger_bridge::close_tab(window_hwnd, &cdp_target_id).await;
            return Err(mcp_error(
                error_codes::ACTION_POSTCONDITION_FAILED,
                format!(
                    "cdp_open_tab refused target {cdp_target_id:?}: Chrome bridge changed the human OS foreground from {human_os_foreground_before_hwnd:?} to requested HWND {window_hwnd:#x} while required_foreground=false"
                ),
            ));
        }
        if opened.chrome_window_focused == Some(true)
            && (opened.target_active || opened.target_highlighted)
        {
            let _ = crate::chrome_debugger_bridge::close_tab(window_hwnd, &cdp_target_id).await;
            return Err(mcp_error(
                error_codes::ACTION_TARGET_INVALID,
                format!(
                    "cdp_open_tab refused target {cdp_target_id:?}: Chrome bridge created an active/highlighted tab in focused Chrome window {chrome_window_id:?}"
                ),
            ));
        }
        let owner_key = self.register_cdp_target_owner(CdpTargetOwner {
            session_id: session_id.to_owned(),
            window_hwnd,
            endpoint: endpoint.clone(),
            chrome_window_id,
            capture_window_hwnd: None,
            cdp_target_id: cdp_target_id.clone(),
            requested_url: redact_url_for_public_readback(requested_url),
            target_url: redact_url_for_public_readback(&opened.url),
            created_at_unix_ms: unix_ms_now(),
        })?;
        let current = TargetWire::Cdp {
            window_hwnd,
            cdp_target_id: cdp_target_id.clone(),
        };
        let previous = self.set_session_target(
            session_id,
            SessionTarget::Cdp {
                window_hwnd,
                cdp_target_id: cdp_target_id.clone(),
            },
        )?;
        tracing::info!(
            code = "CDP_BACKGROUND_TAB_OPENED",
            session_id = %session_id,
            hwnd = window_hwnd,
            endpoint = %endpoint,
            cdp_target_id = %cdp_target_id,
            cdp_owner_key = %owner_key,
            tab_id = opened.tab_id,
            chrome_window_id = chrome_window_id.unwrap_or_default(),
            chrome_window_focused = opened.chrome_window_focused.unwrap_or(false),
            chrome_window_state = %opened.chrome_window_state,
            chrome_window_selection_reason = %opened.chrome_window_selection_reason,
            target_active = opened.target_active,
            target_highlighted = opened.target_highlighted,
            requested_url = %requested_url,
            target_url = %opened.url,
            window_title = %window_title,
            process_name = %process_name,
            target_count_before = opened.target_count_before,
            target_count_after = opened.target_count_after,
            "readback=chrome.tabs.query outcome=target_present"
        );
        self.record_browser_navigation_timeline(BrowserNavigationEvent {
            actor: TimelineActor::Agent {
                session_id: session_id.to_owned(),
            },
            app: Some(process_name.to_owned()),
            source: "cdp_open_tab".to_owned(),
            event: "tool_call".to_owned(),
            action: Some("open".to_owned()),
            url: opened.url.clone(),
            title: opened.title.clone(),
            tab_id: Some(opened.tab_id),
            chrome_window_id,
            window_hwnd: Some(window_hwnd),
            cdp_target_id: Some(cdp_target_id.clone()),
            endpoint: Some(endpoint.clone()),
            transport: Some("chrome_tabs_extension".to_owned()),
            requested_url: Some(requested_url.to_owned()),
            before_url: None,
            before_title: None,
            ready_state: None,
            observed_at_unix_ms: None,
            active: Some(false),
            highlighted: None,
            pinned: None,
        });
        Ok(CdpOpenTabResponse {
            session_id: session_id.to_owned(),
            window_hwnd,
            endpoint,
            chrome_window_id,
            capture_window_hwnd: None,
            chrome_window_focused: opened.chrome_window_focused,
            chrome_window_state: if opened.chrome_window_state.is_empty() {
                None
            } else {
                Some(opened.chrome_window_state)
            },
            human_os_foreground_before_hwnd,
            human_os_foreground_after_hwnd,
            target_active: opened.target_active,
            target_highlighted: opened.target_highlighted,
            requested_url: redact_url_for_public_readback(requested_url),
            cdp_target_id,
            target_type: opened.target_type,
            target_title: opened.title,
            target_url: redact_url_for_public_readback(&opened.url),
            target_attached: opened.target_attached,
            target_count_before: opened.target_count_before,
            target_count_after: opened.target_count_after,
            previous,
            current,
        })
    }

    #[cfg(windows)]
    async fn cdp_target_info_impl(
        &self,
        session_id: &str,
        window_hwnd: i64,
        cdp_target_id: &str,
    ) -> Result<CdpTargetInfoResponse, ErrorData> {
        if let Some(endpoint) = synapse_a11y::endpoint_for_window(window_hwnd) {
            let targets = synapse_a11y::cdp_list_targets(&endpoint)
                .await
                .map_err(|error| {
                    mcp_error(
                        error.code(),
                        format!("cdp_target_info Target.getTargets readback failed: {error}"),
                    )
                })?;
            let Some(target) = targets
                .iter()
                .find(|target| cdp_target_ids_equal(&target.target_id, cdp_target_id))
            else {
                return Err(mcp_error(
                    error_codes::ACTION_POSTCONDITION_FAILED,
                    format!(
                        "cdp_target_info Target.getTargets readback did not contain target {cdp_target_id:?}"
                    ),
                ));
            };
            tracing::info!(
                code = "CDP_TARGET_INFO_READ",
                session_id = %session_id,
                hwnd = window_hwnd,
                endpoint = %endpoint,
                cdp_target_id = %target.target_id,
                target_url = %target.url,
                target_title = %target.title,
                "readback=Target.getTargets outcome=target_present"
            );
            let page_text = raw_cdp_page_text_info(&endpoint, &target.target_id).await;
            let page_vitals = raw_cdp_page_vitals_info(&endpoint, &target.target_id).await;
            return Ok(CdpTargetInfoResponse {
                session_id: session_id.to_owned(),
                window_hwnd,
                transport: "raw_cdp".to_owned(),
                endpoint,
                cdp_target_id: target.target_id.clone(),
                tab_id: None,
                chrome_window_id: None,
                target_type: target.target_type.clone(),
                url: redact_url_for_public_readback(&target.url),
                title: target.title.clone(),
                ready_state: String::new(),
                active: false,
                highlighted: false,
                pinned: false,
                readback_backend: "Target.getTargets".to_owned(),
                backend_tier_used: "cdp".to_owned(),
                required_foreground: false,
                target_candidate_count: u32::try_from(targets.len()).unwrap_or(u32::MAX),
                target_selection_reason: "target_id".to_owned(),
                active_element: None,
                page_text,
                page_vitals,
            });
        }

        let owner =
            self.cdp_target_owner_for_readback("cdp_target_info", session_id, cdp_target_id)?;
        let expected_chrome_window_id = owner.as_ref().and_then(|owner| owner.chrome_window_id);
        let expected_context = validate_target_window_context(window_hwnd).ok();
        let info = crate::chrome_debugger_bridge::target_info(
            window_hwnd,
            cdp_target_id,
            expected_chrome_window_id,
            expected_context
                .as_ref()
                .map(|context| context.window_bounds),
            expected_context
                .as_ref()
                .map(|context| context.window_title.as_str()),
        )
        .await
        .map_err(|error| {
            mcp_error(
                error.code(),
                format!(
                    "cdp_target_info Chrome bridge chrome.tabs.get/readback failed: {}",
                    error.detail()
                ),
            )
        })?;
        let endpoint = info
            .extension_id
            .as_deref()
            .map(chrome_debugger_endpoint)
            .unwrap_or_else(chrome_debugger_default_endpoint);
        if let Some(expected_window_id) = expected_chrome_window_id
            && info.chrome_window_id != Some(expected_window_id)
        {
            return Err(mcp_error(
                error_codes::ACTION_POSTCONDITION_FAILED,
                format!(
                    "cdp_target_info Chrome bridge returned Chrome window {:?} for requested target {:?}, expected Chrome window {}",
                    info.chrome_window_id, cdp_target_id, expected_window_id
                ),
            ));
        }
        tracing::info!(
            code = "CDP_TARGET_INFO_READ",
            session_id = %session_id,
            hwnd = window_hwnd,
            endpoint = %endpoint,
            cdp_target_id = %info.target_id,
            tab_id = info.tab_id,
            target_url = %info.url,
            target_title = %info.title,
            ready_state = %info.ready_state,
            active = info.active,
            highlighted = info.highlighted,
            pinned = info.pinned,
            readback_backend = %info.readback_backend,
            "readback=chrome.tabs.get outcome=target_present"
        );
        Ok(CdpTargetInfoResponse {
            session_id: session_id.to_owned(),
            window_hwnd,
            transport: "chrome_tabs_extension".to_owned(),
            endpoint,
            cdp_target_id: info.target_id,
            tab_id: Some(info.tab_id),
            chrome_window_id: info.chrome_window_id,
            target_type: info.target_type,
            url: redact_url_for_public_readback(&info.url),
            title: info.title,
            ready_state: info.ready_state,
            active: info.active,
            highlighted: info.highlighted,
            pinned: info.pinned,
            readback_backend: if info.readback_backend.trim().is_empty() {
                "chrome.tabs.get".to_owned()
            } else {
                info.readback_backend
            },
            backend_tier_used: "chrome_tabs".to_owned(),
            required_foreground: false,
            target_candidate_count: info.target_candidate_count,
            target_selection_reason: info.target_selection_reason,
            active_element: info.active_element.as_ref().map(chrome_active_element_info),
            page_text: info.page_text.as_ref().map(chrome_page_text_info),
            page_vitals: info.page_vitals.as_ref().map(chrome_page_vitals_info),
        })
    }

    #[cfg(windows)]
    fn resolve_browser_tabs_window_context(
        &self,
        tool: &str,
        session_id: &str,
        window_hwnd: Option<i64>,
        allow_passive_chromium_discovery: bool,
    ) -> Result<BrowserTabsWindowResolution, ErrorData> {
        let explicit_or_session_resolution = if let Some(hwnd) = window_hwnd {
            Some((
                validate_target_window_context(hwnd)?,
                false,
                "explicit_window_hwnd",
                1,
            ))
        } else if let Some(target) = self.session_target(Some(session_id))? {
            let hwnd = match target {
                SessionTarget::Window { hwnd } => hwnd,
                SessionTarget::Cdp { window_hwnd, .. } => window_hwnd,
            };
            Some((
                validate_target_window_context(hwnd)?,
                false,
                "session_target",
                1,
            ))
        } else {
            None
        };
        let (context, used_human_os_foreground_window, discovery_source, candidate_count) =
            if let Some(resolution) = explicit_or_session_resolution {
                resolution
            } else {
                let context = synapse_a11y::current_foreground_context().map_err(|error| {
                    mcp_error(
                        error.code(),
                        format!(
                            "{tool} could not read the current human OS foreground window: {error}"
                        ),
                    )
                })?;
                if synapse_a11y::is_chromium_family(&context.process_name) {
                    (context, true, "human_os_foreground_chromium", 1)
                } else if allow_passive_chromium_discovery {
                    let candidates = passive_chromium_window_candidates(tool, Some(&context))?;
                    match candidates.as_slice() {
                        [candidate] => (
                            candidate.clone(),
                            false,
                            "passive_single_chromium_window",
                            1,
                        ),
                        [] => {
                            return Err(mcp_error(
                                error_codes::ACTION_TARGET_INVALID,
                                format!(
                                    "{tool} resolved non-Chromium human foreground hwnd={:#x} process_name={:?} title={:?}; passive discovery found no visible Chromium windows. remediation=use target operation=list to inspect windows, open or reuse an existing Chrome/Chromium/Edge/Brave/Vivaldi/Opera window, or pass window_hwnd explicitly.",
                                    context.hwnd, context.process_name, context.window_title
                                ),
                            ));
                        }
                        _ => {
                            return Err(mcp_error(
                                error_codes::ACTION_TARGET_INVALID,
                                format!(
                                    "{tool} resolved non-Chromium human foreground hwnd={:#x} process_name={:?} title={:?}; passive discovery found {} Chromium windows and refused to guess. candidates=[{}] remediation=pass window_hwnd from target operation=list or bind a session target first.",
                                    context.hwnd,
                                    context.process_name,
                                    context.window_title,
                                    candidates.len(),
                                    format_chromium_window_candidates(&candidates)
                                ),
                            ));
                        }
                    }
                } else {
                    (context, true, "human_os_foreground_non_chromium", 0)
                }
            };
        if !synapse_a11y::is_chromium_family(&context.process_name) {
            return Err(mcp_error(
                error_codes::ACTION_TARGET_INVALID,
                format!(
                    "{tool} requires a Chromium browser window; resolved hwnd={:#x} process_name={:?} title={:?}",
                    context.hwnd, context.process_name, context.window_title
                ),
            ));
        }
        Ok(BrowserTabsWindowResolution {
            context,
            used_human_os_foreground_window,
            discovery_source,
            chromium_window_candidate_count: candidate_count,
        })
    }

    #[cfg(not(windows))]
    fn resolve_browser_tabs_window_context(
        &self,
        tool: &str,
        _session_id: &str,
        _window_hwnd: Option<i64>,
        _allow_passive_chromium_discovery: bool,
    ) -> Result<BrowserTabsWindowResolution, ErrorData> {
        Err(mcp_error(
            error_codes::A11Y_NOT_AVAILABLE,
            format!("{tool} is only available on Windows in this build"),
        ))
    }

    #[cfg(windows)]
    async fn browser_tabs_dispatch(
        &self,
        session_id: &str,
        window_context: ForegroundContext,
        used_human_os_foreground_window: bool,
        params: &BrowserTabsParams,
    ) -> Result<BrowserTabsResponse, ErrorData> {
        match params.operation {
            BrowserTabsOperation::List => {
                self.browser_tabs_impl(
                    session_id,
                    window_context,
                    used_human_os_foreground_window,
                    BrowserTabsOperation::List,
                    None,
                )
                .await
            }
            BrowserTabsOperation::Select => {
                let mut response = self
                    .browser_tabs_impl(
                        session_id,
                        window_context,
                        used_human_os_foreground_window,
                        BrowserTabsOperation::Select,
                        None,
                    )
                    .await?;
                let requested = params.cdp_target_id.as_deref().ok_or_else(|| {
                    mcp_error(
                        error_codes::TOOL_INTERNAL_ERROR,
                        "browser_tabs operation=select is missing its validated cdp_target_id"
                            .to_string(),
                    )
                })?;
                let selected = response
                    .tabs
                    .iter()
                    .find(|tab| tab.cdp_target_id.eq_ignore_ascii_case(requested))
                    .cloned()
                    .ok_or_else(|| {
                        mcp_error(
                            error_codes::ACTION_TARGET_INVALID,
                            format!(
                                "browser_tabs operation=select could not find target {requested:?} in listed tabs for window {:#x}",
                                response.window_hwnd
                            ),
                        )
                    })?;
                let current = TargetWire::Cdp {
                    window_hwnd: response.window_hwnd,
                    cdp_target_id: selected.cdp_target_id.clone(),
                };
                let previous = self.set_session_target(
                    session_id,
                    SessionTarget::Cdp {
                        window_hwnd: response.window_hwnd,
                        cdp_target_id: selected.cdp_target_id.clone(),
                    },
                )?;
                response.mutation = Some(BrowserTabsMutation {
                    operation: BrowserTabsOperation::Select,
                    requested_cdp_target_id: Some(requested.to_owned()),
                    requested_url: None,
                    previous,
                    current: Some(current),
                    selected_tab: Some(selected),
                    activated_cdp_target_id: None,
                    activated_tab: None,
                    before_active: None,
                    active: None,
                    highlighted: None,
                    activation_visual_readback: None,
                    opened_cdp_target_id: None,
                    closed_cdp_target_id: None,
                    closed: false,
                });
                Ok(response)
            }
            BrowserTabsOperation::Activate => {
                let requested = params.cdp_target_id.as_deref().ok_or_else(|| {
                    mcp_error(
                        error_codes::TOOL_INTERNAL_ERROR,
                        "browser_tabs operation=activate is missing its validated cdp_target_id"
                            .to_string(),
                    )
                })?;
                if synapse_a11y::endpoint_for_window(window_context.hwnd).is_some() {
                    return Err(mcp_error(
                        error_codes::ACTION_TARGET_INVALID,
                        format!(
                            "browser_tabs operation=activate targets the normal Chrome extension bridge, but window {:#x} exposes a raw CDP debug endpoint; use cdp_activate_tab directly for raw-CDP targets",
                            window_context.hwnd
                        ),
                    ));
                }
                let before_response = self
                    .browser_tabs_impl(
                        session_id,
                        window_context.clone(),
                        used_human_os_foreground_window,
                        BrowserTabsOperation::Activate,
                        None,
                    )
                    .await?;
                let before_tab = before_response
                    .tabs
                    .iter()
                    .find(|tab| tab.cdp_target_id.eq_ignore_ascii_case(requested))
                    .cloned()
                    .ok_or_else(|| {
                        mcp_error(
                            error_codes::ACTION_TARGET_INVALID,
                            format!(
                                "browser_tabs operation=activate could not find target {requested:?} in listed tabs for window {:#x}; refusing to activate a tab outside the requested Chrome window",
                                before_response.window_hwnd
                            ),
                        )
                    })?;
                let visual_before = if before_tab.active {
                    None
                } else {
                    Some(browser_tabs_activation_visual_probe(
                        window_context.hwnd,
                        "before_activation",
                    )?)
                };
                let human_os_foreground_before_hwnd = current_human_os_foreground_hwnd();
                let activated = crate::chrome_debugger_bridge::activate_tab(
                    window_context.hwnd,
                    requested,
                    DEFAULT_CDP_NAVIGATE_WAIT_TIMEOUT_MS,
                )
                .await
                .map_err(|error| {
                    mcp_error(
                        error.code(),
                        format!(
                            "browser_tabs operation=activate Chrome bridge chrome.tabs.update({{active:true}}) failed: {}",
                            error.detail()
                        ),
                    )
                })?;
                let human_os_foreground_after_hwnd = current_human_os_foreground_hwnd();
                if background_tab_activation_foregrounded_requested_window(
                    human_os_foreground_before_hwnd,
                    human_os_foreground_after_hwnd,
                    window_context.hwnd,
                ) {
                    return Err(mcp_error(
                        error_codes::ACTION_POSTCONDITION_FAILED,
                        format!(
                            "browser_tabs operation=activate refused target {requested:?}: Chrome bridge changed the human OS foreground from {:?} to requested HWND {:#x} while required_foreground=false",
                            human_os_foreground_before_hwnd, window_context.hwnd
                        ),
                    ));
                }
                if !activated.target_id.eq_ignore_ascii_case(requested) {
                    return Err(mcp_error(
                        error_codes::ACTION_POSTCONDITION_FAILED,
                        format!(
                            "browser_tabs operation=activate postcondition failed: bridge returned target {:?}, expected {requested:?}",
                            activated.target_id
                        ),
                    ));
                }
                if before_tab.chrome_window_id.is_some()
                    && activated.chrome_window_id.is_some()
                    && before_tab.chrome_window_id != activated.chrome_window_id
                {
                    return Err(mcp_error(
                        error_codes::ACTION_POSTCONDITION_FAILED,
                        format!(
                            "browser_tabs operation=activate postcondition failed: activated Chrome window {:?}, expected {:?}",
                            activated.chrome_window_id, before_tab.chrome_window_id
                        ),
                    ));
                }
                if !activated.active || activated.highlighted == Some(false) {
                    return Err(mcp_error(
                        error_codes::ACTION_POSTCONDITION_FAILED,
                        format!(
                            "browser_tabs operation=activate postcondition failed after chrome.tabs.update: active={} highlighted={:?}",
                            activated.active, activated.highlighted
                        ),
                    ));
                }
                let mut response = self
                    .browser_tabs_impl(
                        session_id,
                        window_context.clone(),
                        used_human_os_foreground_window,
                        BrowserTabsOperation::Activate,
                        None,
                    )
                    .await?;
                let activated_tab = response
                    .tabs
                    .iter()
                    .find(|tab| tab.cdp_target_id.eq_ignore_ascii_case(requested))
                    .cloned()
                    .ok_or_else(|| {
                        mcp_error(
                            error_codes::ACTION_POSTCONDITION_FAILED,
                            format!(
                                "browser_tabs operation=activate postcondition failed: target {requested:?} was absent from tabs.query readback after activation"
                            ),
                        )
                    })?;
                if !activated_tab.active || !activated_tab.highlighted {
                    return Err(mcp_error(
                        error_codes::ACTION_POSTCONDITION_FAILED,
                        format!(
                            "browser_tabs operation=activate tabs.query postcondition failed: target {requested:?} active={} highlighted={}",
                            activated_tab.active, activated_tab.highlighted
                        ),
                    ));
                }
                let activation_visual_readback = if before_tab.active {
                    None
                } else {
                    Some(
                        browser_tabs_verify_activation_visualized(
                            window_context.hwnd,
                            &before_tab,
                            &activated_tab,
                            visual_before.as_ref(),
                        )
                        .await?,
                    )
                };
                response.mutation = Some(BrowserTabsMutation {
                    operation: BrowserTabsOperation::Activate,
                    requested_cdp_target_id: Some(requested.to_owned()),
                    requested_url: None,
                    previous: None,
                    current: None,
                    selected_tab: None,
                    activated_cdp_target_id: Some(activated.target_id),
                    activated_tab: Some(activated_tab),
                    before_active: activated.before_active,
                    active: Some(activated.active),
                    highlighted: activated.highlighted,
                    activation_visual_readback,
                    opened_cdp_target_id: None,
                    closed_cdp_target_id: None,
                    closed: false,
                });
                Ok(response)
            }
            BrowserTabsOperation::New => {
                let url = params.url.clone().ok_or_else(|| {
                    mcp_error(
                        error_codes::TOOL_INTERNAL_ERROR,
                        "browser_tabs operation=new is missing its validated url".to_string(),
                    )
                })?;
                if synapse_a11y::endpoint_for_window(window_context.hwnd).is_some() {
                    return Err(mcp_error(
                        error_codes::ACTION_TARGET_INVALID,
                        format!(
                            "browser_tabs operation=new targets the normal Chrome extension bridge, but window {:#x} exposes a raw CDP debug endpoint; use cdp_open_tab directly for raw-CDP targets",
                            window_context.hwnd
                        ),
                    ));
                }
                let opened = self
                    .cdp_open_tab_for_session(
                        CdpOpenTabParams {
                            window_hwnd: Some(window_context.hwnd),
                            url: url.clone(),
                        },
                        session_id,
                    )
                    .await?;
                self.browser_tabs_impl(
                    session_id,
                    window_context,
                    used_human_os_foreground_window,
                    BrowserTabsOperation::New,
                    Some(BrowserTabsMutation {
                        operation: BrowserTabsOperation::New,
                        requested_cdp_target_id: None,
                        requested_url: Some(redact_url_for_public_readback(&url)),
                        previous: opened.previous,
                        current: Some(opened.current),
                        selected_tab: None,
                        activated_cdp_target_id: None,
                        activated_tab: None,
                        before_active: None,
                        active: None,
                        highlighted: None,
                        activation_visual_readback: None,
                        opened_cdp_target_id: Some(opened.cdp_target_id),
                        closed_cdp_target_id: None,
                        closed: false,
                    }),
                )
                .await
            }
            BrowserTabsOperation::Close => {
                let target_id = params.cdp_target_id.clone().ok_or_else(|| {
                    mcp_error(
                        error_codes::TOOL_INTERNAL_ERROR,
                        "browser_tabs operation=close is missing its validated cdp_target_id"
                            .to_string(),
                    )
                })?;
                let explicit_target = SessionTarget::Cdp {
                    window_hwnd: window_context.hwnd,
                    cdp_target_id: target_id.clone(),
                };
                let (owner_key, owner) = self.cdp_target_owner_for_close(
                    session_id,
                    &target_id,
                    Some(&explicit_target),
                )?;
                if owner.window_hwnd != window_context.hwnd {
                    return Err(mcp_error(
                        error_codes::ACTION_TARGET_INVALID,
                        format!(
                            "browser_tabs operation=close target {target_id:?} belongs to window {:#x}, not requested/listed window {:#x}",
                            owner.window_hwnd, window_context.hwnd
                        ),
                    ));
                }
                if synapse_a11y::endpoint_for_window(owner.window_hwnd).is_some() {
                    return Err(mcp_error(
                        error_codes::ACTION_TARGET_INVALID,
                        format!(
                            "browser_tabs operation=close targets the normal Chrome extension bridge, but window {:#x} exposes a raw CDP debug endpoint; use cdp_close_tab directly for raw-CDP targets",
                            owner.window_hwnd
                        ),
                    ));
                }
                let closed = self
                    .cdp_close_tab_impl(session_id, &target_id, &owner_key, owner)
                    .await?;
                self.browser_tabs_impl(
                    session_id,
                    window_context,
                    used_human_os_foreground_window,
                    BrowserTabsOperation::Close,
                    Some(BrowserTabsMutation {
                        operation: BrowserTabsOperation::Close,
                        requested_cdp_target_id: Some(target_id.clone()),
                        requested_url: None,
                        previous: closed.previous,
                        current: closed.current,
                        selected_tab: None,
                        activated_cdp_target_id: None,
                        activated_tab: None,
                        before_active: None,
                        active: None,
                        highlighted: None,
                        activation_visual_readback: None,
                        opened_cdp_target_id: None,
                        closed_cdp_target_id: Some(target_id),
                        closed: closed.closed,
                    }),
                )
                .await
            }
        }
    }

    #[cfg(not(windows))]
    async fn browser_tabs_dispatch(
        &self,
        _session_id: &str,
        _window_context: ForegroundContext,
        _used_human_os_foreground_window: bool,
        _params: &BrowserTabsParams,
    ) -> Result<BrowserTabsResponse, ErrorData> {
        Err(mcp_error(
            error_codes::A11Y_NOT_AVAILABLE,
            "browser_tabs is only available on Windows in this build",
        ))
    }

    #[cfg(windows)]
    async fn browser_tabs_impl(
        &self,
        session_id: &str,
        window_context: ForegroundContext,
        used_human_os_foreground_window: bool,
        operation: BrowserTabsOperation,
        mutation: Option<BrowserTabsMutation>,
    ) -> Result<BrowserTabsResponse, ErrorData> {
        if synapse_a11y::endpoint_for_window(window_context.hwnd).is_some() {
            return Err(mcp_error(
                error_codes::ACTION_TARGET_INVALID,
                format!(
                    "browser_tabs targets the normal Chrome extension bridge, but window {:#x} exposes a raw CDP debug endpoint; use cdp_target_info for Synapse automation-profile targets",
                    window_context.hwnd
                ),
            ));
        }
        let expected_chrome_window_id = self
            .active_cdp_target_owner_for_window("browser_tabs", session_id, window_context.hwnd)?
            .and_then(|owner| owner.chrome_window_id);
        let listed = crate::chrome_debugger_bridge::list_tabs(
            window_context.hwnd,
            expected_chrome_window_id,
            Some(window_context.window_bounds),
            Some(&window_context.window_title),
        )
        .await
        .map_err(|error| {
            mcp_error(
                error.code(),
                format!(
                    "browser_tabs Chrome bridge chrome.tabs.query/readback failed: {}",
                    error.detail()
                ),
            )
        })?;
        let endpoint = listed
            .extension_id
            .as_deref()
            .map(chrome_debugger_endpoint)
            .unwrap_or_else(chrome_debugger_default_endpoint);
        let tabs = listed
            .tabs
            .into_iter()
            .map(|tab| browser_tab_entry(window_context.hwnd, tab))
            .collect::<Vec<_>>();
        let active_tab_count =
            u32::try_from(tabs.iter().filter(|tab| tab.active).count()).unwrap_or(u32::MAX);
        tracing::info!(
            code = "BROWSER_TABS_LISTED",
            session_id = %session_id,
            hwnd = window_context.hwnd,
            endpoint = %endpoint,
            target_count = tabs.len(),
            active_tab_count,
            used_human_os_foreground_window,
            "readback=chrome.tabs.query outcome=tabs_listed"
        );
        Ok(BrowserTabsResponse {
            session_id: session_id.to_owned(),
            operation,
            window_hwnd: window_context.hwnd,
            transport: "chrome_tabs_extension".to_owned(),
            endpoint,
            chrome_window_id: listed.chrome_window_id,
            chrome_window_focused: listed.chrome_window_focused,
            chrome_window_state: if listed.chrome_window_state.is_empty() {
                None
            } else {
                Some(listed.chrome_window_state)
            },
            chrome_window_selection_reason: if listed.chrome_window_selection_reason.is_empty() {
                "passive_hwnd_mapping".to_owned()
            } else {
                listed.chrome_window_selection_reason
            },
            chrome_window_candidate_count: listed.chrome_window_candidate_count,
            chrome_window_non_focused_count: listed.chrome_window_non_focused_count,
            target_count: listed.target_count,
            active_tab_count,
            used_human_os_foreground_window,
            source_of_truth: "chrome.tabs.query via normal Synapse Chrome bridge".to_owned(),
            mutation: mutation.map(redact_browser_tabs_mutation_urls),
            tabs,
        })
    }

    #[cfg(not(windows))]
    async fn browser_tabs_impl(
        &self,
        _session_id: &str,
        _window_context: ForegroundContext,
        _used_human_os_foreground_window: bool,
        _operation: BrowserTabsOperation,
        _mutation: Option<BrowserTabsMutation>,
    ) -> Result<BrowserTabsResponse, ErrorData> {
        Err(mcp_error(
            error_codes::A11Y_NOT_AVAILABLE,
            "browser_tabs is only available on Windows in this build",
        ))
    }

    #[cfg(windows)]
    async fn browser_adopt_active_tab_impl(
        &self,
        session_id: &str,
        window_context: ForegroundContext,
        used_human_os_foreground_window: bool,
    ) -> Result<BrowserAdoptActiveTabResponse, ErrorData> {
        let tabs = self
            .browser_tabs_impl(
                session_id,
                window_context,
                used_human_os_foreground_window,
                BrowserTabsOperation::List,
                None,
            )
            .await?;
        let active_tab = select_single_active_browser_tab(&tabs)?.clone();
        validate_cdp_target_id(&active_tab.cdp_target_id)?;
        let current = TargetWire::Cdp {
            window_hwnd: tabs.window_hwnd,
            cdp_target_id: active_tab.cdp_target_id.clone(),
        };
        let previous = self.set_session_target(
            session_id,
            SessionTarget::Cdp {
                window_hwnd: tabs.window_hwnd,
                cdp_target_id: active_tab.cdp_target_id.clone(),
            },
        )?;
        tracing::info!(
            code = "BROWSER_ACTIVE_TAB_ADOPTED",
            session_id = %session_id,
            hwnd = tabs.window_hwnd,
            endpoint = %tabs.endpoint,
            cdp_target_id = %active_tab.cdp_target_id,
            tab_id = active_tab.tab_id,
            chrome_window_id = active_tab.chrome_window_id.unwrap_or_default(),
            used_human_os_foreground_window = tabs.used_human_os_foreground_window,
            "readback=session_target outcome=adopted_existing_chrome_tab"
        );
        Ok(BrowserAdoptActiveTabResponse {
            session_id: session_id.to_owned(),
            window_hwnd: tabs.window_hwnd,
            transport: tabs.transport,
            endpoint: tabs.endpoint,
            cdp_target_id: active_tab.cdp_target_id.clone(),
            tab_id: active_tab.tab_id,
            chrome_window_id: active_tab.chrome_window_id,
            url: active_tab.url.clone(),
            title: active_tab.title.clone(),
            ready_state: active_tab.ready_state.clone(),
            target_count: tabs.target_count,
            active_tab_count: tabs.active_tab_count,
            chrome_window_selection_reason: tabs.chrome_window_selection_reason,
            used_human_os_foreground_window: tabs.used_human_os_foreground_window,
            source_of_truth: tabs.source_of_truth,
            close_authority: false,
            previous,
            current,
            tab: active_tab,
        })
    }

    #[cfg(not(windows))]
    async fn browser_adopt_active_tab_impl(
        &self,
        _session_id: &str,
        _window_context: ForegroundContext,
        _used_human_os_foreground_window: bool,
    ) -> Result<BrowserAdoptActiveTabResponse, ErrorData> {
        Err(mcp_error(
            error_codes::A11Y_NOT_AVAILABLE,
            "browser_adopt_active_tab is only available on Windows in this build",
        ))
    }

    #[cfg(windows)]
    #[expect(
        clippy::too_many_arguments,
        reason = "browser_evaluate carries page/element scope, args, and CDP flags through the audited choke point"
    )]
    async fn browser_evaluate_impl(
        &self,
        session_id: &str,
        window_hwnd: i64,
        cdp_target_id: &str,
        expression: &str,
        element_id: Option<&str>,
        backend_node_id: Option<i64>,
        args: &[Value],
        await_promise: bool,
        return_by_value: bool,
    ) -> Result<BrowserEvaluateResponse, ErrorData> {
        let Some(endpoint) = synapse_a11y::endpoint_for_window(window_hwnd) else {
            if backend_node_id.is_some() {
                return Err(mcp_error(
                    error_codes::A11Y_CDP_EXTENSION_UNAVAILABLE,
                    format!(
                        "browser_evaluate element scope requires raw CDP for window {window_hwnd:#x}; the normal Chrome bridge exposes only page-scope Runtime.evaluate for chrome-tab targets"
                    ),
                ));
            }
            let evaluated = crate::chrome_debugger_bridge::evaluate_script(
                window_hwnd,
                cdp_target_id,
                expression,
                args,
                await_promise,
                return_by_value,
            )
            .await
            .map_err(|error| {
                mcp_error(
                    error.code(),
                    format!(
                        "browser_evaluate normal Chrome bridge Runtime.evaluate failed: {}",
                        error.detail()
                    ),
                )
            })?;
            let endpoint = evaluated
                .extension_id
                .as_deref()
                .map(chrome_debugger_endpoint)
                .unwrap_or_else(chrome_debugger_default_endpoint);
            tracing::info!(
                code = "CDP_BACKGROUND_EVALUATE",
                session_id = %session_id,
                hwnd = window_hwnd,
                endpoint = %endpoint,
                cdp_target_id = %evaluated.target_id,
                scope = "page",
                element_id = element_id.unwrap_or(""),
                arg_count = args.len(),
                result_type = %evaluated.result_type,
                returned_by_value = evaluated.returned_by_value,
                target_url = %evaluated.url,
                "readback=chrome.scripting.executeScript outcome=evaluated"
            );
            return Ok(BrowserEvaluateResponse {
                session_id: session_id.to_owned(),
                window_hwnd,
                transport: "chrome_tabs_extension".to_owned(),
                endpoint,
                cdp_target_id: evaluated.target_id,
                scope: if evaluated.scope.trim().is_empty() {
                    "page".to_owned()
                } else {
                    evaluated.scope
                },
                element_id: None,
                url: redact_url_for_public_readback(&evaluated.url),
                title: evaluated.title,
                ready_state: evaluated.ready_state,
                result_type: evaluated.result_type,
                result_subtype: evaluated.result_subtype,
                returned_by_value: evaluated.returned_by_value,
                value: evaluated.value,
                description: evaluated.description,
                unserializable_value: evaluated.unserializable_value,
                readback_backend: if evaluated.readback_backend.trim().is_empty() {
                    "chrome.scripting.executeScript".to_owned()
                } else {
                    evaluated.readback_backend
                },
                backend_tier_used: "chrome_tabs".to_owned(),
                required_foreground: false,
            });
        };
        let (evaluated, scope, readback_backend) = if let Some(backend_node_id) = backend_node_id {
            // Element scope: callFunctionOn the resolved node with args; `this`
            // and the first parameter are bound to the element.
            let evaluated = synapse_a11y::cdp_evaluate_on_element(
                &endpoint,
                cdp_target_id,
                backend_node_id,
                expression,
                args,
                await_promise,
                return_by_value,
            )
            .await
            .map_err(|error| {
                mcp_error(
                    error.code(),
                    format!("browser_evaluate raw CDP Runtime.callFunctionOn failed: {error}"),
                )
            })?;
            (evaluated, "element", "Runtime.callFunctionOn")
        } else if args.is_empty() {
            // Page scope, no args: plain Runtime.evaluate of the expression.
            let evaluated = synapse_a11y::cdp_evaluate_expression(
                &endpoint,
                cdp_target_id,
                expression,
                await_promise,
                return_by_value,
            )
            .await
            .map_err(|error| {
                mcp_error(
                    error.code(),
                    format!("browser_evaluate raw CDP Runtime.evaluate failed: {error}"),
                )
            })?;
            (evaluated, "page", "Runtime.evaluate")
        } else {
            // Page scope with args: treat the expression as a function and invoke
            // it with the JSON args injected by value (Playwright evaluate(fn,arg)).
            let arg_list = args
                .iter()
                .map(|arg| {
                    serde_json::to_string(arg).map_err(|error| {
                        mcp_error(
                            error_codes::TOOL_PARAMS_INVALID,
                            format!("browser_evaluate could not serialize arg: {error}"),
                        )
                    })
                })
                .collect::<Result<Vec<_>, _>>()?
                .join(", ");
            let invocation = format!("({expression})({arg_list})");
            let evaluated = synapse_a11y::cdp_evaluate_expression(
                &endpoint,
                cdp_target_id,
                &invocation,
                await_promise,
                return_by_value,
            )
            .await
            .map_err(|error| {
                mcp_error(
                    error.code(),
                    format!(
                        "browser_evaluate raw CDP Runtime.evaluate (with args) failed: {error}"
                    ),
                )
            })?;
            (evaluated, "page", "Runtime.evaluate")
        };
        tracing::info!(
            code = "CDP_BACKGROUND_EVALUATE",
            session_id = %session_id,
            hwnd = window_hwnd,
            endpoint = %endpoint,
            cdp_target_id = %evaluated.target_id,
            scope = scope,
            element_id = element_id.unwrap_or(""),
            arg_count = args.len(),
            result_type = %evaluated.result_type,
            returned_by_value = evaluated.returned_by_value,
            target_url = %evaluated.url,
            "readback={readback_backend} outcome=evaluated"
        );
        Ok(BrowserEvaluateResponse {
            session_id: session_id.to_owned(),
            window_hwnd,
            transport: "raw_cdp".to_owned(),
            endpoint,
            cdp_target_id: evaluated.target_id,
            scope: scope.to_owned(),
            element_id: element_id.map(ToOwned::to_owned),
            url: redact_url_for_public_readback(&evaluated.url),
            title: evaluated.title,
            ready_state: evaluated.ready_state,
            result_type: evaluated.result_type,
            result_subtype: evaluated.result_subtype,
            returned_by_value: evaluated.returned_by_value,
            value: evaluated.value,
            description: evaluated.description,
            unserializable_value: evaluated.unserializable_value,
            readback_backend: readback_backend.to_owned(),
            backend_tier_used: "cdp".to_owned(),
            required_foreground: false,
        })
    }

    #[cfg(not(windows))]
    #[expect(
        clippy::too_many_arguments,
        reason = "signature mirrors the Windows implementation"
    )]
    async fn browser_evaluate_impl(
        &self,
        _session_id: &str,
        _window_hwnd: i64,
        _cdp_target_id: &str,
        _expression: &str,
        _element_id: Option<&str>,
        _backend_node_id: Option<i64>,
        _args: &[Value],
        _await_promise: bool,
        _return_by_value: bool,
    ) -> Result<BrowserEvaluateResponse, ErrorData> {
        Err(mcp_error(
            error_codes::A11Y_NOT_AVAILABLE,
            "browser_evaluate is only available on Windows in this build",
        ))
    }

    #[cfg(windows)]
    async fn browser_expose_binding_impl(
        &self,
        session_id: &str,
        window_hwnd: i64,
        cdp_target_id: &str,
        params: &BrowserExposeBindingParams,
        max_calls: usize,
    ) -> Result<BrowserExposeBindingResponse, ErrorData> {
        let Some(endpoint) = synapse_a11y::endpoint_for_window(window_hwnd) else {
            if cdp_target_id.starts_with("chrome-tab:") {
                let operation = browser_expose_binding_operation_name(params.operation);
                let result = crate::chrome_debugger_bridge::expose_binding(
                    window_hwnd,
                    cdp_target_id,
                    operation,
                    &params.name,
                    params.execution_context_name.as_deref(),
                    params.since_seq,
                    max_calls,
                )
                .await
                .map_err(|error| {
                    mcp_error(
                        error.code(),
                        format!(
                            "browser_expose_binding normal Chrome bridge Runtime.addBinding/bindingCalled failed for target {cdp_target_id:?}: {}",
                            error.detail()
                        ),
                    )
                })?;
                let endpoint = result
                    .extension_id
                    .as_deref()
                    .map(chrome_debugger_endpoint)
                    .unwrap_or_else(chrome_debugger_default_endpoint);
                tracing::info!(
                    code = "CHROME_BRIDGE_BACKGROUND_BINDING",
                    session_id = %session_id,
                    hwnd = window_hwnd,
                    endpoint = %endpoint,
                    cdp_target_id = %result.target_id,
                    operation = ?params.operation,
                    name = %params.name,
                    newly_armed = result.newly_armed,
                    binding_newly_added = result.binding_newly_added,
                    binding_removed = result.binding_removed,
                    returned = result.returned,
                    total_buffered = result.total_buffered,
                    dropped = result.dropped,
                    target_url = %result.url,
                    "readback=chrome.debugger.Runtime.addBinding+Runtime.bindingCalled+Runtime.removeBinding outcome=binding_buffer_read"
                );
                return Ok(BrowserExposeBindingResponse {
                    session_id: session_id.to_owned(),
                    window_hwnd,
                    transport: "chrome_tabs_extension".to_owned(),
                    endpoint,
                    cdp_target_id: result.target_id,
                    operation: params.operation,
                    name: result.name,
                    newly_armed: result.newly_armed,
                    binding_newly_added: result.binding_newly_added,
                    binding_removed: result.binding_removed,
                    armed_at_unix_ms: result.armed_at_unix_ms,
                    binding_active: result.binding_active,
                    active_binding_count: result.active_binding_count,
                    active_binding_names: result.active_binding_names,
                    url: redact_url_for_public_readback(&result.url),
                    title: result.title,
                    ready_state: result.ready_state,
                    calls: result
                        .calls
                        .into_iter()
                        .map(browser_binding_call_from_bridge)
                        .collect(),
                    next_cursor: result.next_cursor,
                    returned: result.returned,
                    total_buffered: result.total_buffered,
                    dropped: result.dropped,
                    readback_backend: if result.readback_backend.trim().is_empty() {
                        "chrome.debugger.Runtime.addBinding+Runtime.bindingCalled+Runtime.removeBinding"
                            .to_owned()
                    } else {
                        result.readback_backend
                    },
                    backend_tier_used: if result.backend_tier_used.trim().is_empty() {
                        "chrome_tabs_extension".to_owned()
                    } else {
                        result.backend_tier_used
                    },
                    required_foreground: result.required_foreground,
                });
            }
            return Err(browser_raw_cdp_required_error(
                "browser_expose_binding",
                window_hwnd,
            ));
        };
        let mut status = synapse_a11y::CdpBindingCaptureStatus {
            newly_armed: false,
            binding_newly_added: false,
            binding_removed: false,
            endpoint: endpoint.clone(),
            cdp_target_id: cdp_target_id.to_owned(),
            name: params.name.clone(),
            armed_at_unix_ms: 0.0,
            capacity: 0,
            binding_active: false,
            active_binding_count: 0,
            active_binding_names: Vec::new(),
        };
        match params.operation {
            BrowserExposeBindingOperation::Add => {
                status = synapse_a11y::binding_capture_add(
                    &endpoint,
                    cdp_target_id,
                    &params.name,
                    params.execution_context_name.as_deref(),
                    synapse_a11y::DEFAULT_BINDING_BUFFER_CAPACITY,
                )
                .await
                .map_err(|error| {
                    mcp_error(
                        error.code(),
                        format!("browser_expose_binding Runtime.addBinding failed: {error}"),
                    )
                })?;
            }
            BrowserExposeBindingOperation::Read => {}
            BrowserExposeBindingOperation::Remove => {
                status =
                    synapse_a11y::binding_capture_remove(&endpoint, cdp_target_id, &params.name)
                        .await
                        .map_err(|error| {
                            mcp_error(
                                error.code(),
                                format!(
                                    "browser_expose_binding Runtime.removeBinding failed: {error}"
                                ),
                            )
                        })?;
            }
        }

        let state =
            synapse_a11y::cdp_evaluate_expression(&endpoint, cdp_target_id, "null", false, true)
                .await
                .map_err(|error| {
                    mcp_error(
                        error.code(),
                        format!("browser_expose_binding page state read-back failed: {error}"),
                    )
                })?;
        let filter = synapse_a11y::CdpBindingReadFilter {
            since_seq: params.since_seq,
            max: max_calls,
        };
        let read =
            synapse_a11y::binding_capture_read(&endpoint, cdp_target_id, &params.name, &filter);
        let read = match read {
            Some(read) => read,
            None if params.operation == BrowserExposeBindingOperation::Read => {
                return Err(mcp_error(
                    error_codes::ACTION_TARGET_INVALID,
                    format!(
                        "browser_expose_binding read requested binding {:?} on target {cdp_target_id}, but no capture is armed; call operation=add first",
                        params.name
                    ),
                ));
            }
            None => synapse_a11y::CdpBindingReadResult {
                calls: Vec::new(),
                next_cursor: 0,
                returned: 0,
                total_buffered: 0,
                dropped: 0,
                armed_at_unix_ms: status.armed_at_unix_ms,
                binding_active: status.binding_active,
                active_binding_count: status.active_binding_count,
                active_binding_names: status.active_binding_names.clone(),
            },
        };
        tracing::info!(
            code = "CDP_BACKGROUND_BINDING",
            session_id = %session_id,
            hwnd = window_hwnd,
            endpoint = %endpoint,
            cdp_target_id = %state.target_id,
            operation = ?params.operation,
            name = %params.name,
            newly_armed = status.newly_armed,
            binding_newly_added = status.binding_newly_added,
            binding_removed = status.binding_removed,
            returned = read.returned,
            total_buffered = read.total_buffered,
            dropped = read.dropped,
            target_url = %state.url,
            "readback=Runtime.addBinding+Runtime.bindingCalled+Runtime.removeBinding outcome=binding_buffer_read"
        );
        Ok(BrowserExposeBindingResponse {
            session_id: session_id.to_owned(),
            window_hwnd,
            transport: "raw_cdp".to_owned(),
            endpoint,
            cdp_target_id: state.target_id,
            operation: params.operation,
            name: params.name.clone(),
            newly_armed: status.newly_armed,
            binding_newly_added: status.binding_newly_added,
            binding_removed: status.binding_removed,
            armed_at_unix_ms: read.armed_at_unix_ms,
            binding_active: read.binding_active,
            active_binding_count: read.active_binding_count,
            active_binding_names: read.active_binding_names,
            url: redact_url_for_public_readback(&state.url),
            title: state.title,
            ready_state: state.ready_state,
            calls: read
                .calls
                .into_iter()
                .map(browser_binding_call_from_entry)
                .collect(),
            next_cursor: read.next_cursor,
            returned: read.returned,
            total_buffered: read.total_buffered,
            dropped: read.dropped,
            readback_backend: "Runtime.addBinding+Runtime.bindingCalled+Runtime.removeBinding"
                .to_owned(),
            backend_tier_used: "cdp".to_owned(),
            required_foreground: false,
        })
    }

    #[cfg(not(windows))]
    async fn browser_expose_binding_impl(
        &self,
        _session_id: &str,
        _window_hwnd: i64,
        _cdp_target_id: &str,
        _params: &BrowserExposeBindingParams,
        _max_calls: usize,
    ) -> Result<BrowserExposeBindingResponse, ErrorData> {
        Err(mcp_error(
            error_codes::A11Y_NOT_AVAILABLE,
            "browser_expose_binding is only available on Windows in this build",
        ))
    }

    #[cfg(windows)]
    async fn browser_add_init_script_impl(
        &self,
        session_id: &str,
        window_hwnd: i64,
        cdp_target_id: &str,
        params: &BrowserAddInitScriptParams,
    ) -> Result<BrowserAddInitScriptResponse, ErrorData> {
        let Some(endpoint) = synapse_a11y::endpoint_for_window(window_hwnd) else {
            if cdp_target_id.starts_with("chrome-tab:") {
                let operation = browser_init_script_operation_name(params.operation);
                let result = crate::chrome_debugger_bridge::init_script(
                    window_hwnd,
                    cdp_target_id,
                    operation,
                    params.source.as_deref(),
                    params.identifier.as_deref(),
                    params.world_name.as_deref(),
                    params.include_command_line_api.unwrap_or(false),
                    params.run_immediately.unwrap_or(false),
                )
                .await
                .map_err(|error| {
                    mcp_error(
                        error.code(),
                        format!(
                            "browser_add_init_script normal Chrome bridge initScript failed for target {cdp_target_id:?}: {}",
                            error.detail()
                        ),
                    )
                })?;
                tracing::info!(
                    code = "CHROME_BRIDGE_BACKGROUND_INIT_SCRIPT",
                    session_id = %session_id,
                    hwnd = window_hwnd,
                    cdp_target_id = %result.target_id,
                    operation = ?params.operation,
                    identifier = %result.identifier,
                    source_len = params.source.as_deref().map(str::len),
                    target_url = %result.url,
                    "readback=chrome.debugger.Page.addScriptToEvaluateOnNewDocument/Page.removeScriptToEvaluateOnNewDocument outcome=init_script_mutated"
                );
                return Ok(BrowserAddInitScriptResponse {
                    session_id: session_id.to_owned(),
                    window_hwnd,
                    transport: "chrome_tabs_extension".to_owned(),
                    endpoint: result
                        .extension_id
                        .as_deref()
                        .map(chrome_debugger_endpoint)
                        .unwrap_or_else(chrome_debugger_default_endpoint),
                    cdp_target_id: result.target_id,
                    operation: params.operation,
                    identifier: result.identifier,
                    source_len: (params.operation == BrowserInitScriptOperation::Add)
                        .then(|| params.source.as_deref().map(str::len))
                        .flatten(),
                    world_name: params.world_name.clone(),
                    include_command_line_api: params.include_command_line_api,
                    run_immediately: params.run_immediately,
                    url: redact_url_for_public_readback(&result.url),
                    title: result.title,
                    ready_state: result.ready_state,
                    readback_backend: if result.readback_backend.trim().is_empty() {
                        "chrome.debugger.Page.addScriptToEvaluateOnNewDocument/Page.removeScriptToEvaluateOnNewDocument".to_owned()
                    } else {
                        result.readback_backend
                    },
                    backend_tier_used: "chrome_tabs_extension".to_owned(),
                    required_foreground: false,
                });
            }
            return Err(browser_raw_cdp_required_error(
                "browser_add_init_script",
                window_hwnd,
            ));
        };
        let result = match params.operation {
            BrowserInitScriptOperation::Add => {
                let source = params.source.as_deref().ok_or_else(|| {
                    mcp_error(
                        error_codes::TOOL_PARAMS_INVALID,
                        "browser_add_init_script operation=add requires source",
                    )
                })?;
                synapse_a11y::cdp_add_init_script_target(
                    &endpoint,
                    cdp_target_id,
                    source,
                    params.world_name.as_deref(),
                    params.include_command_line_api,
                    params.run_immediately,
                )
                .await
                .map_err(|error| {
                    mcp_error(
                        error.code(),
                        format!(
                            "browser_add_init_script raw CDP Page.addScriptToEvaluateOnNewDocument failed: {error}"
                        ),
                    )
                })?
            }
            BrowserInitScriptOperation::Remove => {
                let identifier = params.identifier.as_deref().ok_or_else(|| {
                    mcp_error(
                        error_codes::TOOL_PARAMS_INVALID,
                        "browser_add_init_script operation=remove requires identifier",
                    )
                })?;
                synapse_a11y::cdp_remove_init_script_target(
                    &endpoint,
                    cdp_target_id,
                    identifier,
                )
                .await
                .map_err(|error| {
                    mcp_error(
                        error.code(),
                        format!(
                            "browser_add_init_script raw CDP Page.removeScriptToEvaluateOnNewDocument failed: {error}"
                        ),
                    )
                })?
            }
        };
        tracing::info!(
            code = "CDP_BACKGROUND_INIT_SCRIPT",
            session_id = %session_id,
            hwnd = window_hwnd,
            endpoint = %endpoint,
            cdp_target_id = %result.target_id,
            operation = ?params.operation,
            identifier = %result.identifier,
            source_len = params.source.as_deref().map(str::len),
            target_url = %result.state.url,
            "readback=Page.addScriptToEvaluateOnNewDocument/Page.removeScriptToEvaluateOnNewDocument outcome=init_script_mutated"
        );
        Ok(BrowserAddInitScriptResponse {
            session_id: session_id.to_owned(),
            window_hwnd,
            transport: "raw_cdp".to_owned(),
            endpoint,
            cdp_target_id: result.target_id,
            operation: params.operation,
            identifier: result.identifier,
            source_len: (params.operation == BrowserInitScriptOperation::Add)
                .then(|| params.source.as_deref().map(str::len))
                .flatten(),
            world_name: params.world_name.clone(),
            include_command_line_api: params.include_command_line_api,
            run_immediately: params.run_immediately,
            url: redact_url_for_public_readback(&result.state.url),
            title: result.state.title,
            ready_state: result.state.ready_state,
            readback_backend:
                "Page.addScriptToEvaluateOnNewDocument/Page.removeScriptToEvaluateOnNewDocument"
                    .to_owned(),
            backend_tier_used: "cdp".to_owned(),
            required_foreground: false,
        })
    }

    #[cfg(not(windows))]
    async fn browser_add_init_script_impl(
        &self,
        _session_id: &str,
        _window_hwnd: i64,
        _cdp_target_id: &str,
        _params: &BrowserAddInitScriptParams,
    ) -> Result<BrowserAddInitScriptResponse, ErrorData> {
        Err(mcp_error(
            error_codes::A11Y_NOT_AVAILABLE,
            "browser_add_init_script is only available on Windows in this build",
        ))
    }

    #[cfg(windows)]
    async fn browser_add_tag_impl(
        &self,
        session_id: &str,
        window_hwnd: i64,
        cdp_target_id: &str,
        tool: &str,
        tag_kind: BrowserTagKind,
        source: &ResolvedBrowserTagSource,
        script_type: Option<&str>,
    ) -> Result<BrowserAddTagResponse, ErrorData> {
        let Some(endpoint) = synapse_a11y::endpoint_for_window(window_hwnd) else {
            if cdp_target_id.starts_with("chrome-tab:") {
                let marker = browser_tag_marker(tool, cdp_target_id);
                let expression =
                    build_browser_add_tag_expression(tool, tag_kind, source, script_type, &marker)?;
                let evaluated = crate::chrome_debugger_bridge::evaluate_script(
                    window_hwnd,
                    cdp_target_id,
                    &expression,
                    &[],
                    true,
                    true,
                )
                .await
                .map_err(|error| {
                    mcp_error(
                        error.code(),
                        format!(
                            "{tool} normal Chrome bridge Runtime.evaluate failed for target {cdp_target_id:?}: {}",
                            error.detail()
                        ),
                    )
                })?;
                let payload: BrowserAddTagPayload = serde_json::from_value(evaluated.value.clone())
                    .map_err(|error| {
                        mcp_error(
                            error_codes::OBSERVE_INTERNAL,
                            format!("{tool} bridge payload decode failed: {error}"),
                        )
                    })?;
                tracing::info!(
                    code = "CHROME_BRIDGE_BACKGROUND_TAG_INJECT",
                    session_id = %session_id,
                    hwnd = window_hwnd,
                    cdp_target_id = %evaluated.target_id,
                    tag_name = %payload.tag_name,
                    source_kind = %payload.source_kind,
                    content_len = payload.content_len,
                    element_marker = %payload.element_marker,
                    target_url = %evaluated.url,
                    "readback=chrome.debugger.Runtime.evaluate+tag.onload/onerror outcome=tag_injected"
                );
                return Ok(BrowserAddTagResponse {
                    session_id: session_id.to_owned(),
                    window_hwnd,
                    transport: "chrome_tabs_extension".to_owned(),
                    endpoint: evaluated
                        .extension_id
                        .as_deref()
                        .map(chrome_debugger_endpoint)
                        .unwrap_or_else(chrome_debugger_default_endpoint),
                    cdp_target_id: evaluated.target_id,
                    tag_name: payload.tag_name,
                    source_kind: payload.source_kind,
                    requested_url: redact_url_opt_for_public_readback(payload.requested_url),
                    resolved_url: redact_url_opt_for_public_readback(payload.resolved_url),
                    path: source.path.clone(),
                    script_type: script_type.map(ToOwned::to_owned),
                    content_len: payload.content_len,
                    element_marker: payload.element_marker,
                    url: redact_url_for_public_readback(&evaluated.url),
                    title: evaluated.title,
                    ready_state: evaluated.ready_state,
                    readback_backend: "chrome.debugger.Runtime.evaluate+tag.onload/onerror"
                        .to_owned(),
                    backend_tier_used: "chrome_tabs_extension".to_owned(),
                    required_foreground: false,
                });
            }
            return Err(browser_raw_cdp_required_error(tool, window_hwnd));
        };
        let marker = browser_tag_marker(tool, cdp_target_id);
        let expression =
            build_browser_add_tag_expression(tool, tag_kind, source, script_type, &marker)?;
        let evaluated = synapse_a11y::cdp_evaluate_expression(
            &endpoint,
            cdp_target_id,
            &expression,
            true,
            true,
        )
        .await
        .map_err(|error| {
            mcp_error(
                error.code(),
                format!("{tool} raw CDP Runtime.evaluate failed: {error}"),
            )
        })?;
        let payload: BrowserAddTagPayload = serde_json::from_value(evaluated.value.clone())
            .map_err(|error| {
                mcp_error(
                    error_codes::OBSERVE_INTERNAL,
                    format!("{tool} payload decode failed: {error}"),
                )
            })?;
        tracing::info!(
            code = "CDP_BACKGROUND_TAG_INJECT",
            session_id = %session_id,
            hwnd = window_hwnd,
            endpoint = %endpoint,
            cdp_target_id = %evaluated.target_id,
            tag_name = %payload.tag_name,
            source_kind = %payload.source_kind,
            content_len = payload.content_len,
            element_marker = %payload.element_marker,
            target_url = %evaluated.url,
            "readback=Runtime.evaluate+tag.onload/onerror outcome=tag_injected"
        );
        Ok(BrowserAddTagResponse {
            session_id: session_id.to_owned(),
            window_hwnd,
            transport: "raw_cdp".to_owned(),
            endpoint,
            cdp_target_id: evaluated.target_id,
            tag_name: payload.tag_name,
            source_kind: payload.source_kind,
            requested_url: redact_url_opt_for_public_readback(payload.requested_url),
            resolved_url: redact_url_opt_for_public_readback(payload.resolved_url),
            path: source.path.clone(),
            script_type: script_type.map(ToOwned::to_owned),
            content_len: payload.content_len,
            element_marker: payload.element_marker,
            url: redact_url_for_public_readback(&evaluated.url),
            title: evaluated.title,
            ready_state: evaluated.ready_state,
            readback_backend: "Runtime.evaluate+tag.onload/onerror".to_owned(),
            backend_tier_used: "cdp".to_owned(),
            required_foreground: false,
        })
    }

    #[cfg(not(windows))]
    async fn browser_add_tag_impl(
        &self,
        _session_id: &str,
        _window_hwnd: i64,
        _cdp_target_id: &str,
        tool: &str,
        _tag_kind: BrowserTagKind,
        _source: &ResolvedBrowserTagSource,
        _script_type: Option<&str>,
    ) -> Result<BrowserAddTagResponse, ErrorData> {
        Err(mcp_error(
            error_codes::A11Y_NOT_AVAILABLE,
            format!("{tool} is only available on Windows in this build"),
        ))
    }

    #[cfg(windows)]
    async fn browser_wait_for_impl(
        &self,
        session_id: &str,
        window_hwnd: i64,
        cdp_target_id: &str,
        wait: &NormalizedBrowserWaitForParams,
    ) -> Result<BrowserWaitForResponse, ErrorData> {
        const TOOL: &str = "browser_wait_for";
        let Some(endpoint) = synapse_a11y::endpoint_for_window(window_hwnd) else {
            if cdp_target_id.starts_with("chrome-tab:") {
                let state = browser_wait_for_state_bridge_name(wait.state);
                let waited = crate::chrome_debugger_bridge::wait_for_text(
                    window_hwnd,
                    cdp_target_id,
                    state,
                    wait.text.as_deref(),
                    wait.timeout_ms,
                    wait.polling_interval_ms,
                )
                .await
                .map_err(|error| {
                    mcp_error(
                        error.code(),
                        format!(
                            "browser_wait_for normal bridge waitForText failed for target {cdp_target_id:?}: {}",
                            error.detail()
                        ),
                    )
                })?;
                if waited.timed_out {
                    return Err(mcp_error(
                        error_codes::BROWSER_WAIT_TIMEOUT,
                        format!(
                            "browser_wait_for timed out after {} ms waiting for {:?}; poll_count={} observed_text_len={}",
                            wait.timeout_ms,
                            wait.state,
                            waited.poll_count,
                            waited.observed_text_len
                        ),
                    ));
                }
                tracing::info!(
                    code = "CHROME_BRIDGE_BACKGROUND_WAIT_FOR",
                    session_id = %session_id,
                    hwnd = window_hwnd,
                    cdp_target_id = %waited.target_id,
                    state = ?wait.state,
                    elapsed_ms = waited.elapsed_ms,
                    poll_count = waited.poll_count,
                    target_url = %waited.url,
                    "readback=chrome.scripting.executeScript(page text polling) outcome=wait_satisfied"
                );
                return Ok(BrowserWaitForResponse {
                    session_id: session_id.to_owned(),
                    window_hwnd,
                    transport: "chrome_tabs_extension".to_owned(),
                    endpoint: "chrome_bridge".to_owned(),
                    cdp_target_id: waited.target_id,
                    state: wait.state,
                    text: wait.text.clone(),
                    condition_met: waited.condition_met,
                    elapsed_ms: waited.elapsed_ms,
                    timeout_ms: wait.timeout_ms,
                    polling_interval_ms: wait.polling_interval_ms,
                    poll_count: waited.poll_count,
                    observed_text_len: waited.observed_text_len,
                    url: redact_url_for_public_readback(&waited.url),
                    title: waited.title,
                    ready_state: waited.ready_state,
                    readback_backend: waited.readback_backend,
                    backend_tier_used: "chrome_tabs_extension".to_owned(),
                    required_foreground: false,
                });
            }
            return Err(browser_raw_cdp_required_error(TOOL, window_hwnd));
        };
        let expression = build_browser_wait_for_expression(wait)?;
        let evaluated = synapse_a11y::cdp_evaluate_expression(
            &endpoint,
            cdp_target_id,
            &expression,
            true,
            true,
        )
        .await
        .map_err(|error| {
            mcp_error(
                error.code(),
                format!("browser_wait_for raw CDP Runtime.evaluate failed: {error}"),
            )
        })?;
        let payload: BrowserWaitForPayload = serde_json::from_value(evaluated.value.clone())
            .map_err(|error| {
                mcp_error(
                    error_codes::OBSERVE_INTERNAL,
                    format!("browser_wait_for payload decode failed: {error}"),
                )
            })?;
        if payload.timed_out {
            return Err(mcp_error(
                error_codes::BROWSER_WAIT_TIMEOUT,
                format!(
                    "browser_wait_for timed out after {} ms waiting for {:?}; poll_count={} observed_text_len={}",
                    wait.timeout_ms, wait.state, payload.poll_count, payload.observed_text_len
                ),
            ));
        }
        tracing::info!(
            code = "CDP_BACKGROUND_WAIT_FOR",
            session_id = %session_id,
            hwnd = window_hwnd,
            endpoint = %endpoint,
            cdp_target_id = %evaluated.target_id,
            state = ?wait.state,
            elapsed_ms = payload.elapsed_ms,
            poll_count = payload.poll_count,
            target_url = %evaluated.url,
            "readback=Runtime.evaluate(browser_wait_for) outcome=wait_satisfied"
        );
        Ok(BrowserWaitForResponse {
            session_id: session_id.to_owned(),
            window_hwnd,
            transport: "raw_cdp".to_owned(),
            endpoint,
            cdp_target_id: evaluated.target_id,
            state: wait.state,
            text: wait.text.clone(),
            condition_met: payload.condition_met,
            elapsed_ms: payload.elapsed_ms,
            timeout_ms: wait.timeout_ms,
            polling_interval_ms: wait.polling_interval_ms,
            poll_count: payload.poll_count,
            observed_text_len: payload.observed_text_len,
            url: redact_url_for_public_readback(&evaluated.url),
            title: evaluated.title,
            ready_state: evaluated.ready_state,
            readback_backend: "Runtime.evaluate(browser_wait_for)".to_owned(),
            backend_tier_used: "cdp".to_owned(),
            required_foreground: false,
        })
    }

    #[cfg(not(windows))]
    async fn browser_wait_for_impl(
        &self,
        _session_id: &str,
        _window_hwnd: i64,
        _cdp_target_id: &str,
        _wait: &NormalizedBrowserWaitForParams,
    ) -> Result<BrowserWaitForResponse, ErrorData> {
        Err(mcp_error(
            error_codes::A11Y_NOT_AVAILABLE,
            "browser_wait_for is only available on Windows in this build",
        ))
    }

    #[cfg(windows)]
    async fn browser_wait_for_load_state_impl(
        &self,
        session_id: &str,
        window_hwnd: i64,
        cdp_target_id: &str,
        wait: &NormalizedBrowserWaitForLoadStateParams,
    ) -> Result<BrowserWaitForLoadStateResponse, ErrorData> {
        const TOOL: &str = "browser_wait_for_load_state";
        let Some(endpoint) = synapse_a11y::endpoint_for_window(window_hwnd) else {
            if cdp_target_id.starts_with("chrome-tab:") {
                let state = browser_wait_for_load_state_bridge_name(wait.state);
                let waited = crate::chrome_debugger_bridge::wait_for_load_state(
                    window_hwnd,
                    cdp_target_id,
                    state,
                    wait.timeout_ms,
                )
                .await
                .map_err(|error| {
                    mcp_error(
                        error.code(),
                        format!(
                            "browser_wait_for_load_state normal bridge wait failed for target {cdp_target_id:?}: {}",
                            error.detail()
                        ),
                    )
                })?;
                if waited.timed_out {
                    return Err(mcp_error(
                        error_codes::BROWSER_WAIT_TIMEOUT,
                        format!(
                            "browser_wait_for_load_state timed out after {} ms waiting for {:?}; poll_count={} event_count={} network_event_count={} in_flight_requests={} network_idle_quiet_ms={}",
                            wait.timeout_ms,
                            wait.state,
                            waited.poll_count,
                            waited.event_count,
                            waited.network_event_count,
                            waited.in_flight_requests,
                            waited.network_idle_quiet_ms
                        ),
                    ));
                }
                tracing::info!(
                    code = "CHROME_BRIDGE_BACKGROUND_WAIT_FOR_LOAD_STATE",
                    session_id = %session_id,
                    hwnd = window_hwnd,
                    cdp_target_id = %waited.target_id,
                    state = ?wait.state,
                    elapsed_ms = waited.elapsed_ms,
                    event_count = waited.event_count,
                    network_event_count = waited.network_event_count,
                    max_in_flight_requests = waited.max_in_flight_requests,
                    in_flight_requests = waited.in_flight_requests,
                    target_url = %waited.url,
                    "readback=chrome.webNavigation+chrome.scripting.executeScript(load-state polling) outcome=wait_satisfied"
                );
                return Ok(BrowserWaitForLoadStateResponse {
                    session_id: session_id.to_owned(),
                    window_hwnd,
                    transport: "chrome_tabs_extension".to_owned(),
                    endpoint: "chrome_bridge".to_owned(),
                    cdp_target_id: waited.target_id,
                    state: wait.state,
                    condition_met: waited.condition_met,
                    elapsed_ms: waited.elapsed_ms,
                    timeout_ms: wait.timeout_ms,
                    event_count: waited.event_count,
                    network_event_count: waited.network_event_count,
                    max_in_flight_requests: waited.max_in_flight_requests,
                    in_flight_requests: waited.in_flight_requests,
                    network_idle_quiet_ms: waited.network_idle_quiet_ms,
                    lifecycle_network_idle_seen: waited.lifecycle_network_idle_seen,
                    url: redact_url_for_public_readback(&waited.url),
                    title: waited.title,
                    ready_state: waited.ready_state,
                    readback_backend: waited.readback_backend,
                    backend_tier_used: "chrome_tabs_extension".to_owned(),
                    required_foreground: false,
                });
            }
            return Err(browser_raw_cdp_required_error(TOOL, window_hwnd));
        };
        let requested_state = browser_wait_for_load_state_to_a11y(wait.state);
        let waited = synapse_a11y::cdp_wait_for_load_state(
            &endpoint,
            cdp_target_id,
            requested_state,
            wait.timeout_ms,
        )
        .await
        .map_err(|error| {
            mcp_error(
                error.code(),
                format!("browser_wait_for_load_state raw CDP wait failed: {error}"),
            )
        })?;
        tracing::info!(
            code = "CDP_BACKGROUND_WAIT_FOR_LOAD_STATE",
            session_id = %session_id,
            hwnd = window_hwnd,
            endpoint = %endpoint,
            cdp_target_id = %waited.target_id,
            state = ?wait.state,
            elapsed_ms = waited.elapsed_ms,
            event_count = waited.event_count,
            network_event_count = waited.network_event_count,
            max_in_flight_requests = waited.max_in_flight_requests,
            in_flight_requests = waited.in_flight_requests,
            target_url = %waited.url,
            "readback=Page.lifecycleEvent+Network.buffer(browser_wait_for_load_state) outcome=wait_satisfied"
        );
        Ok(BrowserWaitForLoadStateResponse {
            session_id: session_id.to_owned(),
            window_hwnd,
            transport: "raw_cdp".to_owned(),
            endpoint,
            cdp_target_id: waited.target_id,
            state: wait.state,
            condition_met: true,
            elapsed_ms: waited.elapsed_ms,
            timeout_ms: wait.timeout_ms,
            event_count: waited.event_count,
            network_event_count: waited.network_event_count,
            max_in_flight_requests: waited.max_in_flight_requests,
            in_flight_requests: waited.in_flight_requests,
            network_idle_quiet_ms: waited.network_idle_quiet_ms,
            lifecycle_network_idle_seen: waited.lifecycle_network_idle_seen,
            url: redact_url_for_public_readback(&waited.url),
            title: waited.title,
            ready_state: waited.ready_state,
            readback_backend:
                "Page.lifecycleEvent + Network event buffer(browser_wait_for_load_state)".to_owned(),
            backend_tier_used: "cdp".to_owned(),
            required_foreground: false,
        })
    }

    #[cfg(not(windows))]
    async fn browser_wait_for_load_state_impl(
        &self,
        _session_id: &str,
        _window_hwnd: i64,
        _cdp_target_id: &str,
        _wait: &NormalizedBrowserWaitForLoadStateParams,
    ) -> Result<BrowserWaitForLoadStateResponse, ErrorData> {
        Err(mcp_error(
            error_codes::A11Y_NOT_AVAILABLE,
            "browser_wait_for_load_state is only available on Windows in this build",
        ))
    }

    #[cfg(windows)]
    async fn browser_wait_for_url_impl(
        &self,
        session_id: &str,
        window_hwnd: i64,
        cdp_target_id: &str,
        wait: &NormalizedBrowserWaitForUrlParams,
    ) -> Result<BrowserWaitForUrlResponse, ErrorData> {
        const TOOL: &str = "browser_wait_for_url";
        let Some(endpoint) = synapse_a11y::endpoint_for_window(window_hwnd) else {
            if cdp_target_id.starts_with("chrome-tab:") {
                let match_kind = browser_wait_for_url_match_kind_bridge_name(wait.match_kind);
                let waited = crate::chrome_debugger_bridge::wait_for_url(
                    window_hwnd,
                    cdp_target_id,
                    &wait.url,
                    match_kind,
                    wait.timeout_ms,
                    wait.polling_interval_ms,
                )
                .await
                .map_err(|error| {
                    mcp_error(
                        error.code(),
                        format!(
                            "browser_wait_for_url normal bridge wait failed for target {cdp_target_id:?}: {}",
                            error.detail()
                        ),
                    )
                })?;
                if waited.timed_out {
                    return Err(mcp_error(
                        error_codes::BROWSER_WAIT_TIMEOUT,
                        format!(
                            "browser_wait_for_url timed out after {} ms waiting for {:?} pattern {:?}; poll_count={} navigation_event_count={} last_url={:?}",
                            wait.timeout_ms,
                            wait.match_kind,
                            wait.url,
                            waited.poll_count,
                            waited.navigation_event_count,
                            waited.url
                        ),
                    ));
                }
                tracing::info!(
                    code = "CHROME_BRIDGE_BACKGROUND_WAIT_FOR_URL",
                    session_id = %session_id,
                    hwnd = window_hwnd,
                    cdp_target_id = %waited.target_id,
                    match_kind = ?wait.match_kind,
                    elapsed_ms = waited.elapsed_ms,
                    poll_count = waited.poll_count,
                    navigation_event_count = waited.navigation_event_count,
                    target_url = %waited.url,
                    "readback=chrome.tabs+chrome.webNavigation(browser_wait_for_url) outcome=wait_satisfied"
                );
                return Ok(BrowserWaitForUrlResponse {
                    session_id: session_id.to_owned(),
                    window_hwnd,
                    transport: "chrome_tabs_extension".to_owned(),
                    endpoint: "chrome_bridge".to_owned(),
                    cdp_target_id: waited.target_id,
                    url_pattern: wait.url.clone(),
                    match_kind: wait.match_kind,
                    condition_met: waited.condition_met,
                    elapsed_ms: waited.elapsed_ms,
                    timeout_ms: wait.timeout_ms,
                    polling_interval_ms: wait.polling_interval_ms,
                    poll_count: waited.poll_count,
                    navigation_event_count: waited.navigation_event_count,
                    url: redact_url_for_public_readback(&waited.url),
                    title: waited.title,
                    ready_state: waited.ready_state,
                    readback_backend: waited.readback_backend,
                    backend_tier_used: "chrome_tabs_extension".to_owned(),
                    required_foreground: false,
                });
            }
            return Err(browser_raw_cdp_required_error(TOOL, window_hwnd));
        };
        let waited = synapse_a11y::cdp_wait_for_url(
            &endpoint,
            cdp_target_id,
            &wait.url,
            browser_wait_for_url_match_kind_to_a11y(wait.match_kind),
            wait.timeout_ms,
            wait.polling_interval_ms,
        )
        .await
        .map_err(|error| {
            mcp_error(
                error.code(),
                format!("browser_wait_for_url raw CDP wait failed: {error}"),
            )
        })?;
        tracing::info!(
            code = "CDP_BACKGROUND_WAIT_FOR_URL",
            session_id = %session_id,
            hwnd = window_hwnd,
            endpoint = %endpoint,
            cdp_target_id = %waited.target_id,
            match_kind = ?wait.match_kind,
            elapsed_ms = waited.elapsed_ms,
            poll_count = waited.poll_count,
            navigation_event_count = waited.navigation_event_count,
            target_url = %waited.url,
            "readback=Page.frameNavigated+page_state(browser_wait_for_url) outcome=wait_satisfied"
        );
        Ok(BrowserWaitForUrlResponse {
            session_id: session_id.to_owned(),
            window_hwnd,
            transport: "raw_cdp".to_owned(),
            endpoint,
            cdp_target_id: waited.target_id,
            url_pattern: wait.url.clone(),
            match_kind: wait.match_kind,
            condition_met: true,
            elapsed_ms: waited.elapsed_ms,
            timeout_ms: wait.timeout_ms,
            polling_interval_ms: wait.polling_interval_ms,
            poll_count: waited.poll_count,
            navigation_event_count: waited.navigation_event_count,
            url: redact_url_for_public_readback(&waited.url),
            title: waited.title,
            ready_state: waited.ready_state,
            readback_backend: "Page.frameNavigated + page-state polling(browser_wait_for_url)"
                .to_owned(),
            backend_tier_used: "cdp".to_owned(),
            required_foreground: false,
        })
    }

    #[cfg(not(windows))]
    async fn browser_wait_for_url_impl(
        &self,
        _session_id: &str,
        _window_hwnd: i64,
        _cdp_target_id: &str,
        _wait: &NormalizedBrowserWaitForUrlParams,
    ) -> Result<BrowserWaitForUrlResponse, ErrorData> {
        Err(mcp_error(
            error_codes::A11Y_NOT_AVAILABLE,
            "browser_wait_for_url is only available on Windows in this build",
        ))
    }

    #[cfg(windows)]
    async fn browser_wait_for_request_impl(
        &self,
        session_id: &str,
        window_hwnd: i64,
        cdp_target_id: &str,
        wait: &NormalizedBrowserNetworkWaitParams,
    ) -> Result<BrowserWaitForRequestResponse, ErrorData> {
        const TOOL: &str = "browser_wait_for_request";
        let Some(endpoint) = synapse_a11y::endpoint_for_window(window_hwnd) else {
            if cdp_target_id.starts_with("chrome-tab:") {
                let waited = crate::chrome_debugger_bridge::wait_for_request(
                    window_hwnd,
                    cdp_target_id,
                    wait.url.as_deref(),
                    browser_wait_for_url_match_kind_bridge_name(wait.match_kind),
                    wait.method.as_deref(),
                    wait.resource_type.as_deref(),
                    wait.timeout_ms,
                    wait.polling_interval_ms,
                )
                .await
                .map_err(|error| {
                    mcp_error(
                        error.code(),
                        format!(
                            "browser_wait_for_request normal bridge waitForRequest failed for target {cdp_target_id:?}: {}",
                            error.detail()
                        ),
                    )
                })?;
                if waited.timed_out {
                    return Err(mcp_error(
                        error_codes::BROWSER_WAIT_TIMEOUT,
                        format!(
                            "browser_wait_for_request timed out after {} ms; url_filter={:?} match_kind={:?} method={:?} status={:?} resource_type={:?} poll_count={} event_count={} total_buffered={} dropped={}",
                            wait.timeout_ms,
                            wait.url,
                            wait.match_kind,
                            wait.method,
                            wait.status,
                            wait.resource_type,
                            waited.poll_count,
                            waited.event_count,
                            waited.total_buffered,
                            waited.dropped
                        ),
                    ));
                }
                let matched_entry = waited.matched_entry.ok_or_else(|| {
                    mcp_error(
                        error_codes::TOOL_INTERNAL_ERROR,
                        "browser_wait_for_request normal bridge returned condition_met without matched_entry",
                    )
                })?;
                tracing::info!(
                    code = "CHROME_BRIDGE_BACKGROUND_WAIT_FOR_REQUEST",
                    session_id = %session_id,
                    hwnd = window_hwnd,
                    cdp_target_id = %waited.target_id,
                    request_id = %matched_entry.request_id,
                    elapsed_ms = waited.elapsed_ms,
                    poll_count = waited.poll_count,
                    method = ?matched_entry.method,
                    url = ?matched_entry.url,
                    "readback=chrome.webRequest(buffer) outcome=wait_satisfied"
                );
                return Ok(BrowserWaitForRequestResponse {
                    session_id: session_id.to_owned(),
                    window_hwnd,
                    transport: "chrome_tabs_extension".to_owned(),
                    endpoint: "chrome_bridge".to_owned(),
                    cdp_target_id: waited.target_id,
                    url_pattern: wait.url.clone(),
                    match_kind: wait.match_kind,
                    method: wait.method.clone(),
                    resource_type: wait.resource_type.clone(),
                    condition_met: waited.condition_met,
                    elapsed_ms: waited.elapsed_ms,
                    timeout_ms: wait.timeout_ms,
                    polling_interval_ms: wait.polling_interval_ms,
                    poll_count: waited.poll_count,
                    matched_entry: chrome_bridge_network_entry_to_wire(matched_entry),
                    readback_backend: "chrome.webRequest + in-page fetch/XHR event buffer(browser_wait_for_request)"
                        .to_owned(),
                    backend_tier_used: "chrome_tabs_extension".to_owned(),
                    required_foreground: false,
                });
            }
            return Err(browser_raw_cdp_required_error(TOOL, window_hwnd));
        };
        let (entry, elapsed_ms, poll_count) = self
            .browser_wait_for_network_entry(TOOL, &endpoint, cdp_target_id, wait, false)
            .await?;
        tracing::info!(
            code = "CDP_BACKGROUND_WAIT_FOR_REQUEST",
            session_id = %session_id,
            hwnd = window_hwnd,
            endpoint = %endpoint,
            cdp_target_id,
            request_id = %entry.request_id,
            elapsed_ms,
            poll_count,
            method = ?entry.method,
            url = ?entry.url,
            "readback=Network.requestWillBeSent(buffer) outcome=wait_satisfied"
        );
        Ok(BrowserWaitForRequestResponse {
            session_id: session_id.to_owned(),
            window_hwnd,
            transport: "raw_cdp".to_owned(),
            endpoint,
            cdp_target_id: cdp_target_id.to_owned(),
            url_pattern: wait.url.clone(),
            match_kind: wait.match_kind,
            method: wait.method.clone(),
            resource_type: wait.resource_type.clone(),
            condition_met: true,
            elapsed_ms,
            timeout_ms: wait.timeout_ms,
            polling_interval_ms: wait.polling_interval_ms,
            poll_count,
            matched_entry: browser_network_entry_to_wire(&entry),
            readback_backend: "Network event buffer(browser_wait_for_request)".to_owned(),
            backend_tier_used: "cdp".to_owned(),
            required_foreground: false,
        })
    }

    #[cfg(not(windows))]
    async fn browser_wait_for_request_impl(
        &self,
        _session_id: &str,
        _window_hwnd: i64,
        _cdp_target_id: &str,
        _wait: &NormalizedBrowserNetworkWaitParams,
    ) -> Result<BrowserWaitForRequestResponse, ErrorData> {
        Err(mcp_error(
            error_codes::A11Y_NOT_AVAILABLE,
            "browser_wait_for_request is only available on Windows in this build",
        ))
    }

    #[cfg(windows)]
    async fn browser_wait_for_response_impl(
        &self,
        session_id: &str,
        window_hwnd: i64,
        cdp_target_id: &str,
        wait: &NormalizedBrowserNetworkWaitParams,
    ) -> Result<BrowserWaitForNetworkResponseResponse, ErrorData> {
        const TOOL: &str = "browser_wait_for_response";
        let Some(endpoint) = synapse_a11y::endpoint_for_window(window_hwnd) else {
            if cdp_target_id.starts_with("chrome-tab:") {
                let waited = crate::chrome_debugger_bridge::wait_for_response(
                    window_hwnd,
                    cdp_target_id,
                    wait.url.as_deref(),
                    browser_wait_for_url_match_kind_bridge_name(wait.match_kind),
                    wait.method.as_deref(),
                    wait.status,
                    wait.resource_type.as_deref(),
                    wait.timeout_ms,
                    wait.polling_interval_ms,
                )
                .await
                .map_err(|error| {
                    mcp_error(
                        error.code(),
                        format!(
                            "browser_wait_for_response normal bridge waitForResponse failed for target {cdp_target_id:?}: {}",
                            error.detail()
                        ),
                    )
                })?;
                if waited.timed_out {
                    return Err(mcp_error(
                        error_codes::BROWSER_WAIT_TIMEOUT,
                        format!(
                            "browser_wait_for_response timed out after {} ms; url_filter={:?} match_kind={:?} method={:?} status={:?} resource_type={:?} poll_count={} event_count={} total_buffered={} dropped={}",
                            wait.timeout_ms,
                            wait.url,
                            wait.match_kind,
                            wait.method,
                            wait.status,
                            wait.resource_type,
                            waited.poll_count,
                            waited.event_count,
                            waited.total_buffered,
                            waited.dropped
                        ),
                    ));
                }
                let matched_entry = waited.matched_entry.ok_or_else(|| {
                    mcp_error(
                        error_codes::TOOL_INTERNAL_ERROR,
                        "browser_wait_for_response normal bridge returned condition_met without matched_entry",
                    )
                })?;
                let status = matched_entry.status;
                tracing::info!(
                    code = "CHROME_BRIDGE_BACKGROUND_WAIT_FOR_RESPONSE",
                    session_id = %session_id,
                    hwnd = window_hwnd,
                    cdp_target_id = %waited.target_id,
                    request_id = %matched_entry.request_id,
                    elapsed_ms = waited.elapsed_ms,
                    poll_count = waited.poll_count,
                    status = ?status,
                    method = ?matched_entry.method,
                    url = ?matched_entry.url,
                    "readback=chrome.webRequest(buffer) outcome=wait_satisfied"
                );
                return Ok(BrowserWaitForNetworkResponseResponse {
                    session_id: session_id.to_owned(),
                    window_hwnd,
                    transport: "chrome_tabs_extension".to_owned(),
                    endpoint: "chrome_bridge".to_owned(),
                    cdp_target_id: waited.target_id,
                    url_pattern: wait.url.clone(),
                    match_kind: wait.match_kind,
                    method: wait.method.clone(),
                    status: wait.status,
                    resource_type: wait.resource_type.clone(),
                    condition_met: waited.condition_met,
                    elapsed_ms: waited.elapsed_ms,
                    timeout_ms: wait.timeout_ms,
                    polling_interval_ms: wait.polling_interval_ms,
                    poll_count: waited.poll_count,
                    matched_entry: chrome_bridge_network_entry_to_wire(matched_entry),
                    readback_backend: "chrome.webRequest + in-page fetch/XHR event buffer(browser_wait_for_response)"
                        .to_owned(),
                    backend_tier_used: "chrome_tabs_extension".to_owned(),
                    required_foreground: false,
                });
            }
            return Err(browser_raw_cdp_required_error(TOOL, window_hwnd));
        };
        let (entry, elapsed_ms, poll_count) = self
            .browser_wait_for_network_entry(TOOL, &endpoint, cdp_target_id, wait, true)
            .await?;
        let status = entry.response.as_ref().map(|response| response.status);
        tracing::info!(
            code = "CDP_BACKGROUND_WAIT_FOR_RESPONSE",
            session_id = %session_id,
            hwnd = window_hwnd,
            endpoint = %endpoint,
            cdp_target_id,
            request_id = %entry.request_id,
            elapsed_ms,
            poll_count,
            status = ?status,
            method = ?entry.method,
            url = ?entry.url,
            "readback=Network.responseReceived(buffer) outcome=wait_satisfied"
        );
        Ok(BrowserWaitForNetworkResponseResponse {
            session_id: session_id.to_owned(),
            window_hwnd,
            transport: "raw_cdp".to_owned(),
            endpoint,
            cdp_target_id: cdp_target_id.to_owned(),
            url_pattern: wait.url.clone(),
            match_kind: wait.match_kind,
            method: wait.method.clone(),
            status: wait.status,
            resource_type: wait.resource_type.clone(),
            condition_met: true,
            elapsed_ms,
            timeout_ms: wait.timeout_ms,
            polling_interval_ms: wait.polling_interval_ms,
            poll_count,
            matched_entry: browser_network_entry_to_wire(&entry),
            readback_backend: "Network event buffer(browser_wait_for_response)".to_owned(),
            backend_tier_used: "cdp".to_owned(),
            required_foreground: false,
        })
    }

    #[cfg(not(windows))]
    async fn browser_wait_for_response_impl(
        &self,
        _session_id: &str,
        _window_hwnd: i64,
        _cdp_target_id: &str,
        _wait: &NormalizedBrowserNetworkWaitParams,
    ) -> Result<BrowserWaitForNetworkResponseResponse, ErrorData> {
        Err(mcp_error(
            error_codes::A11Y_NOT_AVAILABLE,
            "browser_wait_for_response is only available on Windows in this build",
        ))
    }

    #[cfg(windows)]
    async fn browser_wait_for_network_entry(
        &self,
        tool: &str,
        endpoint: &str,
        cdp_target_id: &str,
        wait: &NormalizedBrowserNetworkWaitParams,
        require_response: bool,
    ) -> Result<(synapse_a11y::CdpNetworkEntry, u64, u64), ErrorData> {
        let capture_status = synapse_a11y::network_capture_ensure(
            endpoint,
            cdp_target_id,
            synapse_a11y::DEFAULT_NETWORK_BUFFER_CAPACITY,
        )
        .await
        .map_err(|error| {
            mcp_error(
                error.code(),
                format!("{tool} raw CDP network capture failed: {error}"),
            )
        })?;
        let read = synapse_a11y::network_capture_read(
            cdp_target_id,
            &synapse_a11y::CdpNetworkReadFilter {
                max: 0,
                ..Default::default()
            },
        )
        .ok_or_else(|| {
            mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                format!("{tool} network capture was not armed for target {cdp_target_id}"),
            )
        })?;
        let mut since_seq = if capture_status.newly_armed {
            0
        } else {
            read.next_cursor
        };
        let started = Instant::now();
        let timeout = std::time::Duration::from_millis(wait.timeout_ms);
        let polling_interval = std::time::Duration::from_millis(wait.polling_interval_ms);
        let mut poll_count = 0u64;

        loop {
            poll_count = poll_count.saturating_add(1);
            let read = synapse_a11y::network_capture_read(
                cdp_target_id,
                &synapse_a11y::CdpNetworkReadFilter {
                    since_seq: Some(since_seq),
                    max: 0,
                    ..Default::default()
                },
            )
            .ok_or_else(|| {
                mcp_error(
                    error_codes::TOOL_INTERNAL_ERROR,
                    format!("{tool} network capture stopped for target {cdp_target_id}"),
                )
            })?;
            if let Some(entry) = read
                .entries
                .iter()
                .find(|entry| browser_network_entry_matches(entry, wait, require_response))
            {
                return Ok((
                    entry.clone(),
                    duration_millis_u64(started.elapsed()),
                    poll_count,
                ));
            }
            since_seq = read.next_cursor;
            let elapsed = started.elapsed();
            if elapsed >= timeout {
                return Err(mcp_error(
                    error_codes::BROWSER_WAIT_TIMEOUT,
                    format!(
                        "{tool} timed out after {} ms; url_filter={:?} match_kind={:?} method={:?} status={:?} resource_type={:?} poll_count={poll_count}",
                        wait.timeout_ms,
                        wait.url,
                        wait.match_kind,
                        wait.method,
                        wait.status,
                        wait.resource_type,
                    ),
                ));
            }
            tokio::time::sleep(timeout.saturating_sub(elapsed).min(polling_interval)).await;
        }
    }

    #[cfg(windows)]
    async fn browser_wait_for_selector_impl(
        &self,
        session_id: &str,
        window_hwnd: i64,
        cdp_target_id: &str,
        wait: &NormalizedBrowserWaitForSelectorParams,
        root_backend_node_id: Option<i64>,
    ) -> Result<BrowserWaitForSelectorResponse, ErrorData> {
        const TOOL: &str = "browser_wait_for_selector";
        let Some(endpoint) = synapse_a11y::endpoint_for_window(window_hwnd) else {
            if cdp_target_id.starts_with("chrome-tab:") {
                if root_backend_node_id.is_some() {
                    return Err(mcp_error(
                        error_codes::TOOL_PARAMS_INVALID,
                        "browser_wait_for_selector normal Chrome bridge path does not support raw-CDP root backend ids; use a bridge DOM-path root_element_id or omit root scoping",
                    ));
                }
                let locator = serde_json::to_value(&wait.locate).map_err(|error| {
                    mcp_error(
                        error_codes::OBSERVE_INTERNAL,
                        format!(
                            "browser_wait_for_selector normal bridge locator serialization failed: {error}"
                        ),
                    )
                })?;
                let waited = crate::chrome_debugger_bridge::wait_for_selector(
                    window_hwnd,
                    cdp_target_id,
                    locator,
                    wait.limit,
                    browser_wait_for_selector_state_bridge_name(wait.state),
                    wait.timeout_ms,
                    wait.polling_interval_ms,
                )
                .await
                .map_err(|error| {
                    mcp_error(
                        error.code(),
                        format!(
                            "browser_wait_for_selector normal bridge waitForSelector failed for target {cdp_target_id:?}: {}",
                            error.detail()
                        ),
                    )
                })?;
                if waited.timed_out {
                    return Err(mcp_error(
                        error_codes::BROWSER_WAIT_TIMEOUT,
                        format!(
                            "browser_wait_for_selector timed out after {} ms waiting for {:?}; elapsed_ms={} poll_count={} match_count={} returned_count={} visible_count={} truncated={}",
                            wait.timeout_ms,
                            wait.state,
                            waited.elapsed_ms,
                            waited.poll_count,
                            waited.match_count,
                            waited.returned_count,
                            waited.visible_count,
                            waited.truncated
                        ),
                    ));
                }
                tracing::info!(
                    code = "CHROME_BRIDGE_BACKGROUND_WAIT_FOR_SELECTOR",
                    session_id = %session_id,
                    hwnd = window_hwnd,
                    cdp_target_id = %waited.target_id,
                    engine = %waited.engine,
                    state = ?wait.state,
                    match_count = waited.match_count,
                    returned_count = waited.returned_count,
                    visible_count = waited.visible_count,
                    poll_count = waited.poll_count,
                    target_url = %waited.url,
                    "readback=chrome.scripting.executeScript(locator polling) outcome=wait_satisfied"
                );
                return Ok(BrowserWaitForSelectorResponse {
                    session_id: session_id.to_owned(),
                    window_hwnd,
                    transport: "chrome_tabs_extension".to_owned(),
                    endpoint: "chrome_bridge".to_owned(),
                    cdp_target_id: waited.target_id,
                    engine: waited.engine,
                    query: waited.query,
                    state: wait.state,
                    condition_met: waited.condition_met,
                    elapsed_ms: waited.elapsed_ms,
                    timeout_ms: wait.timeout_ms,
                    polling_interval_ms: wait.polling_interval_ms,
                    poll_count: waited.poll_count,
                    match_count: waited.match_count,
                    returned_count: waited.returned_count,
                    visible_count: waited.visible_count,
                    truncated: waited.truncated,
                    element_id: waited.element_id,
                    frame: waited.frame.map(browser_chrome_bridge_located_frame),
                    url: redact_url_for_public_readback(&waited.url),
                    title: waited.title,
                    readback_backend: waited.readback_backend,
                    backend_tier_used: "chrome_tabs_extension".to_owned(),
                    required_foreground: false,
                });
            }
            return Err(browser_raw_cdp_required_error(TOOL, window_hwnd));
        };
        let started = Instant::now();
        let mut poll_count = 0u64;
        loop {
            poll_count = poll_count.saturating_add(1);
            let poll = browser_wait_for_selector_poll(
                &endpoint,
                window_hwnd,
                cdp_target_id,
                wait,
                root_backend_node_id,
            )
            .await?;
            if poll.condition_met {
                let elapsed_ms = duration_millis_u64(started.elapsed());
                tracing::info!(
                    code = "CDP_BACKGROUND_WAIT_FOR_SELECTOR",
                    session_id = %session_id,
                    hwnd = window_hwnd,
                    endpoint = %endpoint,
                    cdp_target_id = %poll.cdp_target_id,
                    engine = %poll.engine,
                    state = ?wait.state,
                    match_count = poll.match_count,
                    returned_count = poll.returned_count,
                    visible_count = poll.visible_count,
                    poll_count,
                    target_url = %poll.url,
                    "readback=cdp_locate+Runtime.callFunctionOn(browser_wait_for_selector) outcome=wait_satisfied"
                );
                return Ok(BrowserWaitForSelectorResponse {
                    session_id: session_id.to_owned(),
                    window_hwnd,
                    transport: "raw_cdp".to_owned(),
                    endpoint,
                    cdp_target_id: poll.cdp_target_id,
                    engine: poll.engine,
                    query: poll.query,
                    state: wait.state,
                    condition_met: true,
                    elapsed_ms,
                    timeout_ms: wait.timeout_ms,
                    polling_interval_ms: wait.polling_interval_ms,
                    poll_count,
                    match_count: poll.match_count,
                    returned_count: poll.returned_count,
                    visible_count: poll.visible_count,
                    truncated: poll.truncated,
                    element_id: poll.element_id,
                    frame: poll.frame,
                    url: redact_url_for_public_readback(&poll.url),
                    title: poll.title,
                    readback_backend:
                        "cdp_locate + Runtime.callFunctionOn(browser_wait_for_selector)".to_owned(),
                    backend_tier_used: "cdp".to_owned(),
                    required_foreground: false,
                });
            }
            if started.elapsed() >= std::time::Duration::from_millis(wait.timeout_ms) {
                let elapsed_ms = duration_millis_u64(started.elapsed());
                return Err(mcp_error(
                    error_codes::BROWSER_WAIT_TIMEOUT,
                    format!(
                        "browser_wait_for_selector timed out after {} ms waiting for {:?}; elapsed_ms={} poll_count={} match_count={} returned_count={} visible_count={} truncated={}",
                        wait.timeout_ms,
                        wait.state,
                        elapsed_ms,
                        poll_count,
                        poll.match_count,
                        poll.returned_count,
                        poll.visible_count,
                        poll.truncated
                    ),
                ));
            }
            tokio::time::sleep(std::time::Duration::from_millis(wait.polling_interval_ms)).await;
        }
    }

    #[cfg(not(windows))]
    async fn browser_wait_for_selector_impl(
        &self,
        _session_id: &str,
        _window_hwnd: i64,
        _cdp_target_id: &str,
        _wait: &NormalizedBrowserWaitForSelectorParams,
        _root_backend_node_id: Option<i64>,
    ) -> Result<BrowserWaitForSelectorResponse, ErrorData> {
        Err(mcp_error(
            error_codes::A11Y_NOT_AVAILABLE,
            "browser_wait_for_selector is only available on Windows in this build",
        ))
    }

    #[cfg(windows)]
    async fn browser_wait_for_function_impl(
        &self,
        session_id: &str,
        window_hwnd: i64,
        cdp_target_id: &str,
        wait: &NormalizedBrowserWaitForFunctionParams,
    ) -> Result<BrowserWaitForFunctionResponse, ErrorData> {
        const TOOL: &str = "browser_wait_for_function";
        let Some(endpoint) = synapse_a11y::endpoint_for_window(window_hwnd) else {
            if cdp_target_id.starts_with("chrome-tab:") {
                let waited = crate::chrome_debugger_bridge::wait_for_function(
                    window_hwnd,
                    cdp_target_id,
                    &wait.expression,
                    wait.args.clone(),
                    wait.timeout_ms,
                    wait.polling_interval_ms,
                )
                .await
                .map_err(|error| {
                    mcp_error(
                        error.code(),
                        format!(
                            "browser_wait_for_function Chrome bridge wait failed: {}",
                            error.detail()
                        ),
                    )
                })?;
                if waited.timed_out {
                    return Err(mcp_error(
                        error_codes::BROWSER_WAIT_TIMEOUT,
                        format!(
                            "browser_wait_for_function timed out after {} ms; poll_count={} value_type={} value_description={:?}",
                            wait.timeout_ms,
                            waited.poll_count,
                            waited.value_type,
                            waited.value_description
                        ),
                    ));
                }
                tracing::info!(
                    code = "CHROME_BRIDGE_BACKGROUND_WAIT_FOR_FUNCTION",
                    session_id = %session_id,
                    hwnd = window_hwnd,
                    cdp_target_id = %waited.target_id,
                    expression_len = wait.expression.len(),
                    arg_count = wait.args.len(),
                    elapsed_ms = waited.elapsed_ms,
                    poll_count = waited.poll_count,
                    value_type = %waited.value_type,
                    target_url = %waited.url,
                    "readback=chrome.scripting.executeScript(MAIN waitForFunction predicate polling) outcome=wait_satisfied"
                );
                return Ok(BrowserWaitForFunctionResponse {
                    session_id: session_id.to_owned(),
                    window_hwnd,
                    transport: "chrome_tabs_extension".to_owned(),
                    endpoint: "chrome_bridge".to_owned(),
                    cdp_target_id: waited.target_id,
                    condition_met: waited.condition_met,
                    elapsed_ms: waited.elapsed_ms,
                    timeout_ms: wait.timeout_ms,
                    polling_interval_ms: wait.polling_interval_ms,
                    poll_count: waited.poll_count,
                    expression_len: if waited.expression_len > 0 {
                        waited.expression_len
                    } else {
                        wait.expression.len()
                    },
                    arg_count: if waited.arg_count > 0 {
                        waited.arg_count
                    } else {
                        wait.args.len()
                    },
                    value: waited.value,
                    value_type: waited.value_type,
                    value_description: waited.value_description,
                    unserializable_value: waited.unserializable_value,
                    url: redact_url_for_public_readback(&waited.url),
                    title: waited.title,
                    ready_state: waited.ready_state,
                    readback_backend: waited.readback_backend,
                    backend_tier_used: "chrome_tabs_extension".to_owned(),
                    required_foreground: false,
                });
            }
            return Err(browser_raw_cdp_required_error(TOOL, window_hwnd));
        };
        let expression = build_browser_wait_for_function_expression(wait)?;
        let evaluated = synapse_a11y::cdp_evaluate_expression(
            &endpoint,
            cdp_target_id,
            &expression,
            true,
            true,
        )
        .await
        .map_err(|error| {
            mcp_error(
                error.code(),
                format!("browser_wait_for_function raw CDP Runtime.evaluate failed: {error}"),
            )
        })?;
        let payload: BrowserWaitForFunctionPayload =
            serde_json::from_value(evaluated.value.clone()).map_err(|error| {
                mcp_error(
                    error_codes::OBSERVE_INTERNAL,
                    format!("browser_wait_for_function payload decode failed: {error}"),
                )
            })?;
        if payload.timed_out {
            return Err(mcp_error(
                error_codes::BROWSER_WAIT_TIMEOUT,
                format!(
                    "browser_wait_for_function timed out after {} ms; poll_count={} value_type={} value_description={:?}",
                    wait.timeout_ms,
                    payload.poll_count,
                    payload.value_type,
                    payload.value_description
                ),
            ));
        }
        tracing::info!(
            code = "CDP_BACKGROUND_WAIT_FOR_FUNCTION",
            session_id = %session_id,
            hwnd = window_hwnd,
            endpoint = %endpoint,
            cdp_target_id = %evaluated.target_id,
            expression_len = wait.expression.len(),
            arg_count = wait.args.len(),
            elapsed_ms = payload.elapsed_ms,
            poll_count = payload.poll_count,
            value_type = %payload.value_type,
            target_url = %evaluated.url,
            "readback=Runtime.evaluate(browser_wait_for_function) outcome=wait_satisfied"
        );
        Ok(BrowserWaitForFunctionResponse {
            session_id: session_id.to_owned(),
            window_hwnd,
            transport: "raw_cdp".to_owned(),
            endpoint,
            cdp_target_id: evaluated.target_id,
            condition_met: payload.condition_met,
            elapsed_ms: payload.elapsed_ms,
            timeout_ms: wait.timeout_ms,
            polling_interval_ms: wait.polling_interval_ms,
            poll_count: payload.poll_count,
            expression_len: wait.expression.len(),
            arg_count: wait.args.len(),
            value: payload.value,
            value_type: payload.value_type,
            value_description: payload.value_description,
            unserializable_value: payload.unserializable_value,
            url: redact_url_for_public_readback(&evaluated.url),
            title: evaluated.title,
            ready_state: evaluated.ready_state,
            readback_backend: "Runtime.evaluate(browser_wait_for_function)".to_owned(),
            backend_tier_used: "cdp".to_owned(),
            required_foreground: false,
        })
    }

    #[cfg(not(windows))]
    async fn browser_wait_for_function_impl(
        &self,
        _session_id: &str,
        _window_hwnd: i64,
        _cdp_target_id: &str,
        _wait: &NormalizedBrowserWaitForFunctionParams,
    ) -> Result<BrowserWaitForFunctionResponse, ErrorData> {
        Err(mcp_error(
            error_codes::A11Y_NOT_AVAILABLE,
            "browser_wait_for_function is only available on Windows in this build",
        ))
    }

    #[cfg(windows)]
    async fn browser_content_impl(
        &self,
        session_id: &str,
        window_hwnd: i64,
        cdp_target_id: &str,
        max_bytes: usize,
    ) -> Result<BrowserContentResponse, ErrorData> {
        let Some(endpoint) = synapse_a11y::endpoint_for_window(window_hwnd) else {
            if cdp_target_id.starts_with("chrome-tab:") {
                let content = crate::chrome_debugger_bridge::page_content(
                    window_hwnd,
                    cdp_target_id,
                    max_bytes,
                )
                .await
                .map_err(|error| {
                    mcp_error(
                        error.code(),
                        format!(
                            "browser_content normal bridge pageContent failed for target {cdp_target_id:?}: {}",
                            error.detail()
                        ),
                    )
                })?;
                tracing::info!(
                    code = "CHROME_BRIDGE_BACKGROUND_CONTENT",
                    session_id = %session_id,
                    hwnd = window_hwnd,
                    cdp_target_id = %content.target_id,
                    html_len = content.html_len,
                    truncated = content.truncated,
                    target_url = %content.url,
                    "readback=chrome.scripting.executeScript(document.documentElement.outerHTML) outcome=content_read"
                );
                return Ok(BrowserContentResponse {
                    session_id: session_id.to_owned(),
                    window_hwnd,
                    transport: "chrome_tabs_extension".to_owned(),
                    endpoint: "chrome-extension://leoocgnkjnplbfdbklajepahofecgfbk/chrome.tabs"
                        .to_owned(),
                    cdp_target_id: content.target_id,
                    url: redact_url_for_public_readback(&content.url),
                    title: content.title,
                    ready_state: content.ready_state,
                    html: content.html,
                    html_len: content.html_len,
                    truncated: content.truncated,
                    max_bytes: content.max_bytes,
                    readback_backend: content.readback_backend,
                    required_foreground: false,
                });
            }
            return Err(browser_raw_cdp_required_error(
                "browser_content",
                window_hwnd,
            ));
        };
        let expression = format!(
            r#"(() => {{
                const max = {max_bytes};
                const html = (document.documentElement && typeof document.documentElement.outerHTML === "string")
                    ? document.documentElement.outerHTML
                    : "";
                return {{ html: html.slice(0, max), html_len: html.length, truncated: html.length > max }};
            }})()"#
        );
        let evaluated = synapse_a11y::cdp_evaluate_expression(
            &endpoint,
            cdp_target_id,
            &expression,
            false,
            true,
        )
        .await
        .map_err(|error| {
            mcp_error(
                error.code(),
                format!("browser_content raw CDP Runtime.evaluate failed: {error}"),
            )
        })?;
        let payload: BrowserContentPayload =
            serde_json::from_value(evaluated.value).map_err(|error| {
                mcp_error(
                    error_codes::OBSERVE_INTERNAL,
                    format!("browser_content payload decode failed: {error}"),
                )
            })?;
        tracing::info!(
            code = "CDP_BACKGROUND_CONTENT",
            session_id = %session_id,
            hwnd = window_hwnd,
            endpoint = %endpoint,
            cdp_target_id = %evaluated.target_id,
            html_len = payload.html_len,
            truncated = payload.truncated,
            target_url = %evaluated.url,
            "readback=Runtime.evaluate(document.documentElement.outerHTML) outcome=content_read"
        );
        Ok(BrowserContentResponse {
            session_id: session_id.to_owned(),
            window_hwnd,
            transport: "raw_cdp".to_owned(),
            endpoint,
            cdp_target_id: evaluated.target_id,
            url: redact_url_for_public_readback(&evaluated.url),
            title: evaluated.title,
            ready_state: evaluated.ready_state,
            html: payload.html,
            html_len: payload.html_len,
            truncated: payload.truncated,
            max_bytes,
            readback_backend: "Runtime.evaluate(document.documentElement.outerHTML)".to_owned(),
            required_foreground: false,
        })
    }

    #[cfg(not(windows))]
    async fn browser_content_impl(
        &self,
        _session_id: &str,
        _window_hwnd: i64,
        _cdp_target_id: &str,
        _max_bytes: usize,
    ) -> Result<BrowserContentResponse, ErrorData> {
        Err(mcp_error(
            error_codes::A11Y_NOT_AVAILABLE,
            "browser_content is only available on Windows in this build",
        ))
    }

    #[cfg(windows)]
    async fn browser_set_content_impl(
        &self,
        session_id: &str,
        window_hwnd: i64,
        cdp_target_id: &str,
        html: &str,
        wait_timeout_ms: u64,
    ) -> Result<BrowserSetContentResponse, ErrorData> {
        let Some(endpoint) = synapse_a11y::endpoint_for_window(window_hwnd) else {
            if cdp_target_id.starts_with("chrome-tab:") {
                let set =
                    crate::chrome_debugger_bridge::set_content(
                        window_hwnd,
                        cdp_target_id,
                        html,
                        wait_timeout_ms,
                        Some(session_id),
                    )
                    .await
                    .map_err(|error| {
                        mcp_error(
                            error.code(),
                            format!(
                                "browser_set_content normal bridge setContent failed for target {cdp_target_id:?}: {}",
                                error.detail()
                            ),
                        )
                    })?;
                tracing::info!(
                    code = "CHROME_BRIDGE_BACKGROUND_SET_CONTENT",
                    session_id = %session_id,
                    hwnd = window_hwnd,
                    cdp_target_id = %set.target_id,
                    html_len = set.html_len,
                    before_url = %set.before_url,
                    after_url = %set.after_url,
                    seeded_url = %set.seeded_url,
                    ready_state = %set.ready_state,
                    "readback=chrome.scripting.executeScript(document.open/write/close)+chrome.tabs.get outcome=content_set"
                );
                return Ok(BrowserSetContentResponse {
                    session_id: session_id.to_owned(),
                    window_hwnd,
                    transport: "chrome_tabs_extension".to_owned(),
                    endpoint: "chrome-extension://leoocgnkjnplbfdbklajepahofecgfbk/chrome.tabs"
                        .to_owned(),
                    cdp_target_id: set.target_id,
                    frame_id: set
                        .frame_id
                        .map(|value| value.to_string())
                        .unwrap_or_else(|| "0".to_owned()),
                    html_len: set.html_len,
                    before_url: redact_url_for_public_readback(&set.before_url),
                    before_title: set.before_title,
                    after_url: redact_url_for_public_readback(&set.after_url),
                    after_title: set.after_title,
                    ready_state: set.ready_state,
                    history_current_index: set.history_current_index,
                    history_entry_count: set.history_entry_count,
                    seeded_url: if set.seeded_url.is_empty() {
                        None
                    } else {
                        Some(redact_url_for_public_readback(&set.seeded_url))
                    },
                    seeded_from_url: if set.seeded_from_url.is_empty() {
                        None
                    } else {
                        Some(redact_url_for_public_readback(&set.seeded_from_url))
                    },
                    seeded_reason: if set.seeded_reason.is_empty() {
                        None
                    } else {
                        Some(set.seeded_reason)
                    },
                    readback_backend: set.readback_backend,
                    backend_tier_used: "chrome_tabs_extension".to_owned(),
                    required_foreground: false,
                });
            }
            return Err(browser_raw_cdp_required_error(
                "browser_set_content",
                window_hwnd,
            ));
        };
        let set = synapse_a11y::cdp_set_document_content_target(
            &endpoint,
            cdp_target_id,
            html,
            wait_timeout_ms,
        )
        .await
        .map_err(|error| {
            mcp_error(
                error.code(),
                format!("browser_set_content raw CDP Page.setDocumentContent failed: {error}"),
            )
        })?;
        tracing::info!(
            code = "CDP_BACKGROUND_SET_CONTENT",
            session_id = %session_id,
            hwnd = window_hwnd,
            endpoint = %endpoint,
            cdp_target_id = %set.target_id,
            frame_id = %set.frame_id,
            html_len = set.html_len,
            before_url = %set.before.url,
            after_url = %set.after.url,
            ready_state = %set.after.ready_state,
            "readback=Page.setDocumentContent+Runtime.evaluate outcome=content_set"
        );
        Ok(BrowserSetContentResponse {
            session_id: session_id.to_owned(),
            window_hwnd,
            transport: "raw_cdp".to_owned(),
            endpoint,
            cdp_target_id: set.target_id,
            frame_id: set.frame_id,
            html_len: set.html_len,
            before_url: redact_url_for_public_readback(&set.before.url),
            before_title: set.before.title,
            after_url: redact_url_for_public_readback(&set.after.url),
            after_title: set.after.title,
            ready_state: set.after.ready_state,
            history_current_index: set.after.history_current_index,
            history_entry_count: set.after.history_entry_count,
            seeded_url: None,
            seeded_from_url: None,
            seeded_reason: None,
            readback_backend: "Page.setDocumentContent+Runtime.evaluate".to_owned(),
            backend_tier_used: "cdp".to_owned(),
            required_foreground: false,
        })
    }

    #[cfg(not(windows))]
    async fn browser_set_content_impl(
        &self,
        _session_id: &str,
        _window_hwnd: i64,
        _cdp_target_id: &str,
        _html: &str,
        _wait_timeout_ms: u64,
    ) -> Result<BrowserSetContentResponse, ErrorData> {
        Err(mcp_error(
            error_codes::A11Y_NOT_AVAILABLE,
            "browser_set_content is only available on Windows in this build",
        ))
    }

    #[cfg(windows)]
    async fn browser_console_messages_impl(
        &self,
        session_id: &str,
        window_hwnd: i64,
        cdp_target_id: &str,
        params: &BrowserConsoleMessagesParams,
        max_messages: usize,
    ) -> Result<BrowserConsoleMessagesResponse, ErrorData> {
        let Some(endpoint) = synapse_a11y::endpoint_for_window(window_hwnd) else {
            return Err(browser_raw_cdp_required_error(
                "browser_console_messages",
                window_hwnd,
            ));
        };
        // Arm (idempotent) the persistent per-target console capture. The first
        // call establishes the listener; later calls reuse the live one.
        let status = synapse_a11y::console_capture_ensure(
            &endpoint,
            cdp_target_id,
            synapse_a11y::DEFAULT_CONSOLE_BUFFER_CAPACITY,
        )
        .await
        .map_err(|error| {
            mcp_error(
                error.code(),
                format!("browser_console_messages capture arm failed: {error}"),
            )
        })?;
        // Independent page-context read-back (FSV source-of-truth correlation):
        // the url/title/readyState the messages were captured against.
        let state =
            synapse_a11y::cdp_evaluate_expression(&endpoint, cdp_target_id, "null", false, true)
                .await
                .map_err(|error| {
                    mcp_error(
                        error.code(),
                        format!("browser_console_messages page state read-back failed: {error}"),
                    )
                })?;
        let filter = synapse_a11y::ConsoleReadFilter {
            since_seq: params.since_seq,
            level: params.level.as_deref(),
            source: params.source.as_deref(),
            text_contains: params.text_contains.as_deref(),
            max: max_messages,
        };
        let read = synapse_a11y::console_capture_read(cdp_target_id, &filter).ok_or_else(|| {
            mcp_error(
                error_codes::OBSERVE_INTERNAL,
                format!(
                    "browser_console_messages: capture for target {cdp_target_id} is not armed immediately after ensure succeeded"
                ),
            )
        })?;
        let messages: Vec<ConsoleMessage> = read
            .entries
            .into_iter()
            .map(console_message_from_entry)
            .collect();
        tracing::info!(
            code = "CDP_BACKGROUND_CONSOLE",
            session_id = %session_id,
            hwnd = window_hwnd,
            endpoint = %endpoint,
            cdp_target_id = %state.target_id,
            newly_armed = status.newly_armed,
            returned = read.returned,
            total_buffered = read.total_buffered,
            dropped = read.dropped,
            target_url = %state.url,
            "readback=Runtime.consoleAPICalled+Runtime.exceptionThrown+Log.entryAdded outcome=console_messages_read"
        );
        Ok(BrowserConsoleMessagesResponse {
            session_id: session_id.to_owned(),
            window_hwnd,
            transport: "raw_cdp".to_owned(),
            endpoint,
            cdp_target_id: state.target_id,
            newly_armed: status.newly_armed,
            armed_at_unix_ms: status.armed_at_unix_ms,
            messages,
            next_cursor: read.next_cursor,
            returned: read.returned,
            total_buffered: read.total_buffered,
            dropped: read.dropped,
            readback_backend: "Runtime.consoleAPICalled+Runtime.exceptionThrown+Log.entryAdded"
                .to_owned(),
            required_foreground: false,
        })
    }

    #[cfg(not(windows))]
    async fn browser_console_messages_impl(
        &self,
        _session_id: &str,
        _window_hwnd: i64,
        _cdp_target_id: &str,
        _params: &BrowserConsoleMessagesParams,
        _max_messages: usize,
    ) -> Result<BrowserConsoleMessagesResponse, ErrorData> {
        Err(mcp_error(
            error_codes::A11Y_NOT_AVAILABLE,
            "browser_console_messages is only available on Windows in this build",
        ))
    }

    #[cfg(windows)]
    async fn browser_inspect_impl(
        &self,
        session_id: &str,
        window_hwnd: i64,
        cdp_target_id: &str,
        element_id: &str,
        backend_node_id: i64,
        max_html_bytes: usize,
    ) -> Result<BrowserInspectResponse, ErrorData> {
        let Some(endpoint) = synapse_a11y::endpoint_for_window(window_hwnd) else {
            return Err(browser_raw_cdp_required_error(
                "browser_inspect",
                window_hwnd,
            ));
        };
        let evaluated = synapse_a11y::cdp_evaluate_on_element(
            &endpoint,
            cdp_target_id,
            backend_node_id,
            BROWSER_INSPECT_FUNCTION,
            std::slice::from_ref(&json!(max_html_bytes)),
            false,
            true,
        )
        .await
        .map_err(|error| {
            mcp_error(
                error.code(),
                format!("browser_inspect raw CDP Runtime.callFunctionOn failed: {error}"),
            )
        })?;
        let mut inspection: ElementInspection =
            serde_json::from_value(evaluated.value).map_err(|error| {
                mcp_error(
                    error_codes::OBSERVE_INTERNAL,
                    format!("browser_inspect payload decode failed: {error}"),
                )
            })?;
        let actionability =
            synapse_a11y::cdp_actionability(&endpoint, cdp_target_id, backend_node_id)
                .await
                .map_err(|error| {
                    mcp_error(
                        error.code(),
                        format!("browser_inspect actionability readback failed: {error}"),
                    )
                })?;
        inspection.actionability = Some(serde_json::to_value(&actionability).map_err(|error| {
            mcp_error(
                error_codes::OBSERVE_INTERNAL,
                format!("browser_inspect actionability payload encode failed: {error}"),
            )
        })?);
        tracing::info!(
            code = "CDP_BACKGROUND_INSPECT",
            session_id = %session_id,
            hwnd = window_hwnd,
            endpoint = %endpoint,
            cdp_target_id = %evaluated.target_id,
            element_id = element_id,
            tag_name = %inspection.tag_name,
            is_visible = inspection.is_visible,
            action_ready = actionability.action_ready,
            receives_events = actionability.receives_events,
            target_url = %evaluated.url,
            "readback=Runtime.callFunctionOn+DOM.getBoxModel+DOM.getNodeForLocation+elementFromPoint outcome=element_inspected"
        );
        Ok(BrowserInspectResponse {
            session_id: session_id.to_owned(),
            window_hwnd,
            transport: "raw_cdp".to_owned(),
            endpoint,
            cdp_target_id: evaluated.target_id,
            element_id: element_id.to_owned(),
            url: redact_url_for_public_readback(&evaluated.url),
            title: evaluated.title,
            ready_state: evaluated.ready_state,
            element: inspection,
            readback_backend:
                "Runtime.callFunctionOn + DOM.getBoxModel + DOM.getNodeForLocation + elementFromPoint"
                    .to_owned(),
            required_foreground: false,
        })
    }

    #[cfg(windows)]
    async fn browser_inspect_bridge_impl(
        &self,
        session_id: &str,
        window_hwnd: i64,
        cdp_target_id: &str,
        element_id: &str,
        max_html_bytes: usize,
    ) -> Result<BrowserInspectResponse, ErrorData> {
        let inspected = crate::chrome_debugger_bridge::inspect_element(
            window_hwnd,
            cdp_target_id,
            element_id,
            max_html_bytes,
        )
        .await
        .map_err(|error| {
            mcp_error(
                error.code(),
                format!(
                    "browser_inspect normal bridge inspectElement failed for target {cdp_target_id:?}: {}",
                    error.detail()
                ),
            )
        })?;
        let element: ElementInspection =
            serde_json::from_value(inspected.element).map_err(|error| {
                mcp_error(
                    error_codes::OBSERVE_INTERNAL,
                    format!("browser_inspect normal bridge payload decode failed: {error}"),
                )
            })?;
        tracing::info!(
            code = "CHROME_BRIDGE_BACKGROUND_INSPECT",
            session_id = %session_id,
            hwnd = window_hwnd,
            cdp_target_id = %inspected.target_id,
            element_id = element_id,
            tag_name = %element.tag_name,
            is_visible = element.is_visible,
            action_ready = element
                .actionability
                .as_ref()
                .and_then(|value| value.get("action_ready"))
                .and_then(|value| value.as_bool())
                .unwrap_or(false),
            receives_events = element
                .actionability
                .as_ref()
                .and_then(|value| value.get("receives_events"))
                .and_then(|value| value.as_bool())
                .unwrap_or(false),
            target_url = %inspected.url,
            "readback=chrome.scripting.executeScript+elementFromPoint outcome=element_inspected"
        );
        Ok(BrowserInspectResponse {
            session_id: session_id.to_owned(),
            window_hwnd,
            transport: "chrome_tabs_extension".to_owned(),
            endpoint: "chrome_bridge".to_owned(),
            cdp_target_id: inspected.target_id,
            element_id: element_id.to_owned(),
            url: redact_url_for_public_readback(&inspected.url),
            title: inspected.title,
            ready_state: inspected.ready_state,
            element,
            readback_backend: inspected.readback_backend,
            required_foreground: false,
        })
    }

    #[cfg(not(windows))]
    async fn browser_inspect_impl(
        &self,
        _session_id: &str,
        _window_hwnd: i64,
        _cdp_target_id: &str,
        _element_id: &str,
        _backend_node_id: i64,
        _max_html_bytes: usize,
    ) -> Result<BrowserInspectResponse, ErrorData> {
        Err(mcp_error(
            error_codes::A11Y_NOT_AVAILABLE,
            "browser_inspect is only available on Windows in this build",
        ))
    }

    #[cfg(not(windows))]
    async fn browser_inspect_bridge_impl(
        &self,
        _session_id: &str,
        _window_hwnd: i64,
        _cdp_target_id: &str,
        _element_id: &str,
        _max_html_bytes: usize,
    ) -> Result<BrowserInspectResponse, ErrorData> {
        Err(mcp_error(
            error_codes::A11Y_NOT_AVAILABLE,
            "browser_inspect is only available on Windows in this build",
        ))
    }

    #[cfg(windows)]
    async fn browser_scroll_into_view_impl(
        &self,
        session_id: &str,
        window_hwnd: i64,
        cdp_target_id: &str,
        element_id: &str,
        backend_node_id: i64,
    ) -> Result<BrowserScrollIntoViewResponse, ErrorData> {
        let Some(endpoint) = synapse_a11y::endpoint_for_window(window_hwnd) else {
            return Err(browser_raw_cdp_required_error(
                "browser_scroll_into_view",
                window_hwnd,
            ));
        };
        let scrolled =
            synapse_a11y::cdp_scroll_into_view_node(&endpoint, cdp_target_id, backend_node_id)
                .await
                .map_err(|error| {
                    mcp_error(
                        error.code(),
                        format!("browser_scroll_into_view raw CDP scroll failed: {error}"),
                    )
                })?;
        tracing::info!(
            code = "CDP_BACKGROUND_SCROLL_INTO_VIEW",
            session_id = %session_id,
            hwnd = window_hwnd,
            endpoint = %endpoint,
            cdp_target_id = %scrolled.target_id,
            element_id = element_id,
            window_scroll_changed = scrolled.window_scroll_changed,
            container_scroll_changed = scrolled.container_scroll_changed,
            node_fully_in_viewport_after = scrolled.node_fully_in_viewport_after,
            "readback=DOM.scrollIntoViewIfNeeded+Runtime.callFunctionOn+DOM.getBoxModel outcome=element_scrolled_into_view"
        );
        Ok(BrowserScrollIntoViewResponse {
            session_id: session_id.to_owned(),
            window_hwnd,
            transport: "raw_cdp".to_owned(),
            endpoint,
            cdp_target_id: scrolled.target_id.clone(),
            element_id: element_id.to_owned(),
            scroll: serde_json::to_value(&scrolled).map_err(|error| {
                mcp_error(
                    error_codes::OBSERVE_INTERNAL,
                    format!("browser_scroll_into_view payload encode failed: {error}"),
                )
            })?,
            readback_backend:
                "DOM.scrollIntoViewIfNeeded + Runtime.callFunctionOn + DOM.getBoxModel".to_owned(),
            required_foreground: false,
        })
    }

    #[cfg(windows)]
    async fn browser_scroll_into_view_bridge_impl(
        &self,
        session_id: &str,
        window_hwnd: i64,
        cdp_target_id: &str,
        element_id: &str,
    ) -> Result<BrowserScrollIntoViewResponse, ErrorData> {
        let scrolled =
            crate::chrome_debugger_bridge::scroll_into_view(window_hwnd, cdp_target_id, element_id)
                .await
                .map_err(|error| {
                    mcp_error(
                        error.code(),
                        format!(
                            "browser_scroll_into_view normal bridge scrollIntoView failed for target {cdp_target_id:?}: {}",
                            error.detail()
                        ),
                    )
                })?;
        tracing::info!(
            code = "CHROME_BRIDGE_BACKGROUND_SCROLL_INTO_VIEW",
            session_id = %session_id,
            hwnd = window_hwnd,
            cdp_target_id = %scrolled.target_id,
            element_id = element_id,
            window_scroll_changed = scrolled
                .scroll
                .get("window_scroll_changed")
                .and_then(|value| value.as_bool())
                .unwrap_or(false),
            container_scroll_changed = scrolled
                .scroll
                .get("container_scroll_changed")
                .and_then(|value| value.as_bool())
                .unwrap_or(false),
            node_fully_in_viewport_after = scrolled
                .scroll
                .get("node_fully_in_viewport_after")
                .and_then(|value| value.as_bool())
                .unwrap_or(false),
            "readback=chrome.scripting.executeScript(Element.scrollIntoView)+geometry outcome=element_scrolled_into_view"
        );
        Ok(BrowserScrollIntoViewResponse {
            session_id: session_id.to_owned(),
            window_hwnd,
            transport: "chrome_tabs_extension".to_owned(),
            endpoint: "chrome_bridge".to_owned(),
            cdp_target_id: scrolled.target_id,
            element_id: element_id.to_owned(),
            scroll: scrolled.scroll,
            readback_backend: scrolled.readback_backend,
            required_foreground: false,
        })
    }

    #[cfg(not(windows))]
    async fn browser_scroll_into_view_impl(
        &self,
        _session_id: &str,
        _window_hwnd: i64,
        _cdp_target_id: &str,
        _element_id: &str,
        _backend_node_id: i64,
    ) -> Result<BrowserScrollIntoViewResponse, ErrorData> {
        Err(mcp_error(
            error_codes::A11Y_NOT_AVAILABLE,
            "browser_scroll_into_view is only available on Windows in this build",
        ))
    }

    #[cfg(not(windows))]
    async fn browser_scroll_into_view_bridge_impl(
        &self,
        _session_id: &str,
        _window_hwnd: i64,
        _cdp_target_id: &str,
        _element_id: &str,
    ) -> Result<BrowserScrollIntoViewResponse, ErrorData> {
        Err(mcp_error(
            error_codes::A11Y_NOT_AVAILABLE,
            "browser_scroll_into_view is only available on Windows in this build",
        ))
    }

    #[cfg(windows)]
    #[allow(
        clippy::too_many_lines,
        reason = "one selector-engine request build + structured readback log + response"
    )]
    async fn browser_locate_impl(
        &self,
        session_id: &str,
        window_hwnd: i64,
        cdp_target_id: &str,
        params: &BrowserLocateParams,
        root_backend_node_id: Option<i64>,
        limit: usize,
    ) -> Result<BrowserLocateResponse, ErrorData> {
        let Some(endpoint) = synapse_a11y::endpoint_for_window(window_hwnd) else {
            if cdp_target_id.starts_with("chrome-tab:") {
                let locator = serde_json::to_value(params).map_err(|error| {
                    mcp_error(
                        error_codes::OBSERVE_INTERNAL,
                        format!(
                            "browser_locate normal bridge locator serialization failed: {error}"
                        ),
                    )
                })?;
                let located = crate::chrome_debugger_bridge::locate_elements(
                    window_hwnd,
                    cdp_target_id,
                    locator,
                    limit,
                )
                .await
                .map_err(|error| {
                    mcp_error(
                        error.code(),
                        format!(
                            "browser_locate normal bridge locateElements failed for target {cdp_target_id:?}: {}",
                            error.detail()
                        ),
                    )
                })?;
                tracing::info!(
                    code = "CHROME_BRIDGE_BACKGROUND_LOCATE",
                    session_id = %session_id,
                    hwnd = window_hwnd,
                    cdp_target_id = %located.target_id,
                    engine = %located.engine,
                    match_count = located.match_count,
                    returned_count = located.element_ids.len(),
                    root_scoped = params.root_element_id.is_some(),
                    target_url = %located.url,
                    "readback=chrome.scripting.executeScript locator outcome=located"
                );
                return Ok(BrowserLocateResponse {
                    session_id: session_id.to_owned(),
                    window_hwnd,
                    transport: "chrome_tabs_extension".to_owned(),
                    endpoint: "chrome_bridge".to_owned(),
                    cdp_target_id: located.target_id,
                    engine: located.engine,
                    query: located.query,
                    match_count: located.match_count,
                    returned_count: located.returned_count,
                    truncated: located.truncated,
                    element_ids: located.element_ids,
                    frame: located.frame.map(browser_chrome_bridge_located_frame),
                    url: redact_url_for_public_readback(&located.url),
                    title: located.title,
                    readback_backend: located.readback_backend,
                    required_foreground: false,
                });
            }
            return Err(browser_raw_cdp_required_error(
                "browser_locate",
                window_hwnd,
            ));
        };
        let scope = resolve_browser_locate_scope(
            &endpoint,
            window_hwnd,
            cdp_target_id,
            params.frame.as_ref(),
        )
        .await?;
        if scope.frame_requested && !scope.frame_resolved {
            let frame = scope.frame_readback;
            return Ok(BrowserLocateResponse {
                session_id: session_id.to_owned(),
                window_hwnd,
                transport: "raw_cdp".to_owned(),
                endpoint,
                cdp_target_id: cdp_target_id.to_owned(),
                engine: browser_locate_engine_to_a11y(params.engine)
                    .as_str()
                    .to_owned(),
                query: params.query.clone(),
                match_count: 0,
                returned_count: 0,
                truncated: false,
                element_ids: Vec::new(),
                frame,
                url: scope
                    .page_url
                    .map(|url| redact_url_for_public_readback(&url))
                    .unwrap_or_default(),
                title: scope.page_title.unwrap_or_default(),
                readback_backend: "Page.getFrameTree frame locator".to_owned(),
                required_foreground: false,
            });
        }

        let mut request = browser_locate_cdp_request(params, root_backend_node_id, limit);
        request.frame_id = scope.frame_id.clone();
        let located = synapse_a11y::cdp_locate(&endpoint, &scope.cdp_target_id, request)
            .await
            .map_err(|error| {
                mcp_error(
                    error.code(),
                    format!("browser_locate raw CDP selector resolution failed: {error}"),
                )
            })?;
        let element_ids: Vec<String> = located
            .backend_node_ids
            .iter()
            .map(|backend| {
                synapse_a11y::cdp_element_id_for_target(window_hwnd, &located.target_id, *backend)
                    .to_string()
            })
            .collect();
        let readback_backend = if params.engine == BrowserLocateEngine::Role {
            "Accessibility.queryAXTree + AX state filter"
        } else {
            "injected selector engine + Runtime.getProperties + DOM.describeNode"
        };
        tracing::info!(
            code = "CDP_BACKGROUND_LOCATE",
            session_id = %session_id,
            hwnd = window_hwnd,
            endpoint = %endpoint,
            cdp_target_id = %located.target_id,
            engine = %located.engine,
            match_count = located.match_count,
            returned_count = element_ids.len(),
            root_scoped = root_backend_node_id.is_some(),
            frame_id = ?located.frame_id,
            target_url = %located.url,
            "readback={readback_backend} outcome=located"
        );
        Ok(BrowserLocateResponse {
            session_id: session_id.to_owned(),
            window_hwnd,
            transport: "raw_cdp".to_owned(),
            endpoint,
            cdp_target_id: located.target_id,
            engine: located.engine,
            query: located.query,
            match_count: located.match_count,
            returned_count: element_ids.len(),
            truncated: located.truncated,
            element_ids,
            frame: scope.frame_readback,
            url: redact_url_for_public_readback(&located.url),
            title: located.title,
            readback_backend: readback_backend.to_owned(),
            required_foreground: false,
        })
    }

    #[cfg(not(windows))]
    async fn browser_locate_impl(
        &self,
        _session_id: &str,
        _window_hwnd: i64,
        _cdp_target_id: &str,
        _params: &BrowserLocateParams,
        _root_backend_node_id: Option<i64>,
        _limit: usize,
    ) -> Result<BrowserLocateResponse, ErrorData> {
        Err(mcp_error(
            error_codes::A11Y_NOT_AVAILABLE,
            "browser_locate is only available on Windows in this build",
        ))
    }

    #[cfg(windows)]
    async fn cdp_open_tab_raw_impl(
        &self,
        session_id: &str,
        window_hwnd: i64,
        endpoint: &str,
        requested_url: &str,
        window_title: &str,
        process_name: &str,
    ) -> Result<CdpOpenTabResponse, ErrorData> {
        let human_os_foreground_before_hwnd = current_human_os_foreground_hwnd();
        let opened = synapse_a11y::cdp_open_background_tab(endpoint, requested_url)
            .await
            .map_err(|error| {
                mcp_error(
                    error.code(),
                    format!("cdp_open_tab Target.createTarget/readback failed: {error}"),
                )
            })?;
        let human_os_foreground_after_hwnd = current_human_os_foreground_hwnd();
        let cdp_target_id = opened.target.target_id.clone();
        let owner_key = self.register_cdp_target_owner(CdpTargetOwner {
            session_id: session_id.to_owned(),
            window_hwnd,
            endpoint: endpoint.to_owned(),
            chrome_window_id: None,
            capture_window_hwnd: None,
            cdp_target_id: cdp_target_id.clone(),
            requested_url: redact_url_for_public_readback(requested_url),
            target_url: redact_url_for_public_readback(&opened.target.url),
            created_at_unix_ms: unix_ms_now(),
        })?;
        let current = TargetWire::Cdp {
            window_hwnd,
            cdp_target_id: cdp_target_id.clone(),
        };
        let previous = self.set_session_target(
            session_id,
            SessionTarget::Cdp {
                window_hwnd,
                cdp_target_id: cdp_target_id.clone(),
            },
        )?;
        tracing::info!(
            code = "CDP_BACKGROUND_TAB_OPENED",
            session_id = %session_id,
            hwnd = window_hwnd,
            endpoint = %endpoint,
            cdp_target_id = %cdp_target_id,
            cdp_owner_key = %owner_key,
            requested_url = %requested_url,
            target_url = %opened.target.url,
            window_title = %window_title,
            process_name = %process_name,
            "readback=Target.getTargets outcome=target_present"
        );
        self.record_browser_navigation_timeline(BrowserNavigationEvent {
            actor: TimelineActor::Agent {
                session_id: session_id.to_owned(),
            },
            app: Some(process_name.to_owned()),
            source: "cdp_open_tab".to_owned(),
            event: "tool_call".to_owned(),
            action: Some("open".to_owned()),
            url: opened.target.url.clone(),
            title: opened.target.title.clone(),
            tab_id: None,
            chrome_window_id: None,
            window_hwnd: Some(window_hwnd),
            cdp_target_id: Some(cdp_target_id.clone()),
            endpoint: Some(endpoint.to_owned()),
            transport: Some("raw_cdp".to_owned()),
            requested_url: Some(requested_url.to_owned()),
            before_url: None,
            before_title: None,
            ready_state: None,
            observed_at_unix_ms: None,
            active: Some(false),
            highlighted: None,
            pinned: None,
        });
        Ok(CdpOpenTabResponse {
            session_id: session_id.to_owned(),
            window_hwnd,
            endpoint: endpoint.to_owned(),
            chrome_window_id: None,
            capture_window_hwnd: None,
            chrome_window_focused: None,
            chrome_window_state: None,
            human_os_foreground_before_hwnd,
            human_os_foreground_after_hwnd,
            target_active: false,
            target_highlighted: false,
            requested_url: redact_url_for_public_readback(requested_url),
            cdp_target_id,
            target_type: opened.target.target_type,
            target_title: opened.target.title,
            target_url: redact_url_for_public_readback(&opened.target.url),
            target_attached: opened.target.attached,
            target_count_before: opened.target_count_before,
            target_count_after: opened.target_count_after,
            previous,
            current,
        })
    }

    #[cfg(not(windows))]
    async fn cdp_target_info_impl(
        &self,
        _session_id: &str,
        _window_hwnd: i64,
        _cdp_target_id: &str,
    ) -> Result<CdpTargetInfoResponse, ErrorData> {
        Err(mcp_error(
            error_codes::A11Y_NOT_AVAILABLE,
            "cdp_target_info is only available on Windows in this build",
        ))
    }

    #[cfg(not(windows))]
    async fn cdp_open_tab_impl(
        &self,
        _session_id: &str,
        _window_hwnd: i64,
        _requested_url: &str,
        _window_title: &str,
        _process_name: &str,
    ) -> Result<CdpOpenTabResponse, ErrorData> {
        Err(mcp_error(
            error_codes::A11Y_NOT_AVAILABLE,
            "cdp_open_tab is only available on Windows in this build",
        ))
    }

    #[cfg(windows)]
    async fn cdp_close_tab_impl(
        &self,
        session_id: &str,
        cdp_target_id: &str,
        owner_key: &str,
        owner: CdpTargetOwner,
    ) -> Result<CdpCloseTabResponse, ErrorData> {
        if is_chrome_debugger_endpoint(&owner.endpoint) {
            let closed = match crate::chrome_debugger_bridge::close_tab(
                owner.window_hwnd,
                cdp_target_id,
            )
            .await
            {
                Ok(closed) => closed,
                Err(error)
                    if Self::chrome_bridge_close_target_already_absent(
                        error.detail(),
                        cdp_target_id,
                    ) =>
                {
                    let listed = crate::chrome_debugger_bridge::list_tabs(
                            owner.window_hwnd,
                            owner.chrome_window_id,
                            None,
                            None,
                        )
                        .await
                        .map_err(|readback_error| {
                            mcp_error(
                                readback_error.code(),
                                format!(
                                    "cdp_close_tab Chrome debugger reported target {cdp_target_id:?} already absent, but chrome.tabs.query/readback failed; leaving persisted owner row {owner_key:?} visible: close_error={}; readback_error={}",
                                    error.detail(),
                                    readback_error.detail()
                                ),
                            )
                        })?;
                    if listed
                        .tabs
                        .iter()
                        .any(|tab| tab.target_id.eq_ignore_ascii_case(cdp_target_id))
                    {
                        return Err(mcp_error(
                            error_codes::ACTION_TARGET_INVALID,
                            format!(
                                "cdp_close_tab Chrome debugger reported target {cdp_target_id:?} already absent, but chrome.tabs.query/readback still returned it in window {:#x}; leaving persisted owner row {owner_key:?} visible for retry",
                                owner.window_hwnd
                            ),
                        ));
                    }
                    let _removed = self.remove_cdp_target_owner(owner_key)?;
                    let previous =
                        self.clear_session_cdp_target_if_matches(session_id, cdp_target_id)?;
                    let current = self.get_session_target_wire(session_id)?;
                    let claim_released = self.release_closed_cdp_target_claim(
                        session_id,
                        owner.window_hwnd,
                        cdp_target_id,
                    )?;
                    tracing::info!(
                        code = "CDP_BACKGROUND_TAB_OWNER_RECLAIMED_ALREADY_ABSENT",
                        session_id = %session_id,
                        hwnd = owner.window_hwnd,
                        endpoint = %owner.endpoint,
                        cdp_target_id = %cdp_target_id,
                        cdp_owner_key = %owner_key,
                        requested_url = %owner.requested_url,
                        target_url = %owner.target_url,
                        owner_created_at_unix_ms = owner.created_at_unix_ms,
                        target_count_before = listed.target_count,
                        target_count_after = listed.target_count,
                        target_claim_released = claim_released,
                        close_error = %error.detail(),
                        "readback=chrome.tabs.query outcome=already_absent_owner_row_reclaimed"
                    );
                    return Ok(CdpCloseTabResponse {
                        session_id: session_id.to_owned(),
                        window_hwnd: owner.window_hwnd,
                        endpoint: owner.endpoint,
                        cdp_target_id: cdp_target_id.to_owned(),
                        closed: false,
                        target_count_before: listed.target_count,
                        target_count_after: listed.target_count,
                        previous,
                        current,
                    });
                }
                Err(error) => {
                    return Err(mcp_error(
                        error.code(),
                        format!(
                            "cdp_close_tab Chrome debugger chrome.tabs.remove/readback failed: {}",
                            error.detail()
                        ),
                    ));
                }
            };
            let _removed = self.remove_cdp_target_owner(owner_key)?;
            let previous = self.clear_session_cdp_target_if_matches(session_id, cdp_target_id)?;
            let current = self.get_session_target_wire(session_id)?;
            let claim_released =
                self.release_closed_cdp_target_claim(session_id, owner.window_hwnd, cdp_target_id)?;
            tracing::info!(
                code = "CDP_BACKGROUND_TAB_CLOSED",
                session_id = %session_id,
                hwnd = owner.window_hwnd,
                endpoint = %owner.endpoint,
                cdp_target_id = %closed.target_id,
                cdp_owner_key = %owner_key,
                tab_id = closed.tab_id,
                requested_url = %owner.requested_url,
                target_url = %owner.target_url,
                owner_created_at_unix_ms = owner.created_at_unix_ms,
                target_count_before = closed.target_count_before,
                target_count_after = closed.target_count_after,
                target_claim_released = claim_released,
                "readback=chrome.tabs.query outcome=target_absent"
            );
            return Ok(CdpCloseTabResponse {
                session_id: session_id.to_owned(),
                window_hwnd: owner.window_hwnd,
                endpoint: owner.endpoint,
                cdp_target_id: closed.target_id,
                closed: true,
                target_count_before: closed.target_count_before,
                target_count_after: closed.target_count_after,
                previous,
                current,
            });
        }

        let closed = synapse_a11y::cdp_close_target(&owner.endpoint, cdp_target_id)
            .await
            .map_err(|error| {
                mcp_error(
                    error.code(),
                    format!("cdp_close_tab Target.closeTarget/readback failed: {error}"),
                )
            })?;
        let _removed = self.remove_cdp_target_owner(owner_key)?;
        let previous = self.clear_session_cdp_target_if_matches(session_id, cdp_target_id)?;
        let current = self.get_session_target_wire(session_id)?;
        let claim_released =
            self.release_closed_cdp_target_claim(session_id, owner.window_hwnd, cdp_target_id)?;
        tracing::info!(
            code = "CDP_BACKGROUND_TAB_CLOSED",
            session_id = %session_id,
            hwnd = owner.window_hwnd,
            endpoint = %owner.endpoint,
            cdp_target_id = %cdp_target_id,
            cdp_owner_key = %owner_key,
            requested_url = %owner.requested_url,
            target_url = %owner.target_url,
            owner_created_at_unix_ms = owner.created_at_unix_ms,
            target_claim_released = claim_released,
            "readback=Target.getTargets outcome=target_absent"
        );
        Ok(CdpCloseTabResponse {
            session_id: session_id.to_owned(),
            window_hwnd: owner.window_hwnd,
            endpoint: owner.endpoint,
            cdp_target_id: closed.target_id,
            closed: true,
            target_count_before: closed.target_count_before,
            target_count_after: closed.target_count_after,
            previous,
            current,
        })
    }

    #[cfg(windows)]
    fn chrome_bridge_close_target_already_absent(detail: &str, target_id: &str) -> bool {
        detail.contains("targetIdHint")
            && detail.contains(target_id)
            && detail.contains("did not match any chrome.tabs tab id")
    }

    #[cfg(windows)]
    async fn cdp_navigate_tab_impl(
        &self,
        source_tool: &str,
        session_id: &str,
        window_hwnd: i64,
        cdp_target_id: &str,
        action: CdpNavigateAction,
        requested_url: Option<&str>,
        wait_timeout_ms: u64,
        ignore_cache: bool,
    ) -> Result<CdpNavigateTabResponse, ErrorData> {
        if let Some(endpoint) = synapse_a11y::endpoint_for_window(window_hwnd) {
            let raw_action = raw_cdp_navigation_action(action);
            let navigated = synapse_a11y::cdp_navigate_page_target(
                &endpoint,
                cdp_target_id,
                raw_action,
                requested_url,
                wait_timeout_ms,
                ignore_cache,
            )
            .await
            .map_err(|error| {
                mcp_error(
                    cdp_navigation_error_code(error.code()),
                    format!("cdp_navigate_tab raw Page command/readback failed: {error}"),
                )
            })?;
            tracing::info!(
                code = "CDP_BACKGROUND_TAB_NAVIGATED",
                session_id = %session_id,
                hwnd = window_hwnd,
                endpoint = %endpoint,
                cdp_target_id = %navigated.target_id,
                action = %navigated.action,
                before_url = %navigated.before.url,
                after_url = %navigated.after.url,
                "readback=Page.getNavigationHistory+Runtime.evaluate outcome=target_navigated"
            );
            self.record_browser_navigation_timeline(BrowserNavigationEvent {
                actor: TimelineActor::Agent {
                    session_id: session_id.to_owned(),
                },
                app: Some("chrome.exe".to_owned()),
                source: source_tool.to_owned(),
                event: "tool_call".to_owned(),
                action: Some(navigated.action.clone()),
                url: navigated.after.url.clone(),
                title: navigated.after.title.clone(),
                tab_id: None,
                chrome_window_id: None,
                window_hwnd: Some(window_hwnd),
                cdp_target_id: Some(navigated.target_id.clone()),
                endpoint: Some(endpoint.clone()),
                transport: Some("raw_cdp".to_owned()),
                requested_url: navigated.requested_url.clone(),
                before_url: Some(navigated.before.url.clone()),
                before_title: Some(navigated.before.title.clone()),
                ready_state: Some(navigated.after.ready_state.clone()),
                observed_at_unix_ms: None,
                active: Some(false),
                highlighted: None,
                pinned: None,
            });
            return Ok(redact_cdp_navigate_tab_response_urls(
                CdpNavigateTabResponse {
                    session_id: session_id.to_owned(),
                    window_hwnd,
                    transport: "raw_cdp".to_owned(),
                    endpoint,
                    cdp_target_id: navigated.target_id,
                    action,
                    requested_url: navigated.requested_url,
                    before_url: navigated.before.url,
                    before_title: navigated.before.title,
                    after_url: navigated.after.url,
                    after_title: navigated.after.title,
                    ready_state: navigated.after.ready_state,
                    history_current_index: navigated.after.history_current_index,
                    history_entry_count: navigated.after.history_entry_count,
                    history_readback_source: "Page.getNavigationHistory".to_owned(),
                    readback_backend: "Runtime.evaluate+Page.getNavigationHistory".to_owned(),
                    navigation_error_text: navigated.navigation_error_text,
                    is_download: navigated.is_download,
                    download_status: None,
                    download_id: None,
                    download_url: None,
                    download_final_url: None,
                    download_filename: None,
                    download_state: None,
                    download_match_reason: None,
                    backend_tier_used: "cdp".to_owned(),
                    required_foreground: false,
                    target_candidate_count: 0,
                    target_selection_reason: "target_id".to_owned(),
                },
            ));
        }

        let action_wire = cdp_navigate_action_wire(action);
        let navigated = crate::chrome_debugger_bridge::navigate_tab(
            window_hwnd,
            cdp_target_id,
            action_wire,
            requested_url,
            wait_timeout_ms,
            ignore_cache,
            Some(session_id),
        )
        .await
        .map_err(|error| {
            mcp_error(
                cdp_navigation_error_code(error.code()),
                format!(
                    "cdp_navigate_tab Chrome debugger Page command/readback failed: {}",
                    error.detail()
                ),
            )
        })?;
        let endpoint = navigated
            .extension_id
            .as_deref()
            .map(chrome_debugger_endpoint)
            .unwrap_or_else(chrome_debugger_default_endpoint);
        tracing::info!(
            code = "CDP_BACKGROUND_TAB_NAVIGATED",
            session_id = %session_id,
            hwnd = window_hwnd,
            endpoint = %endpoint,
            cdp_target_id = %navigated.target_id,
            tab_id = navigated.tab_id,
            action = %navigated.action,
            before_url = %navigated.before_url,
            after_url = %navigated.after_url,
            target_candidate_count = navigated.target_candidate_count,
            target_selection_reason = %navigated.target_selection_reason,
            readback_backend = %navigated.readback_backend,
            history_readback_source = %navigated.history_readback_source,
            "readback=chrome.tabs.get outcome=target_navigated"
        );
        self.record_browser_navigation_timeline(BrowserNavigationEvent {
            actor: TimelineActor::Agent {
                session_id: session_id.to_owned(),
            },
            app: Some("chrome.exe".to_owned()),
            source: source_tool.to_owned(),
            event: "tool_call".to_owned(),
            action: Some(navigated.action.clone()),
            url: navigated.after_url.clone(),
            title: navigated.after_title.clone(),
            tab_id: Some(navigated.tab_id),
            chrome_window_id: None,
            window_hwnd: Some(window_hwnd),
            cdp_target_id: Some(navigated.target_id.clone()),
            endpoint: Some(endpoint.clone()),
            transport: Some("chrome_tabs_extension".to_owned()),
            requested_url: navigated.requested_url.clone(),
            before_url: Some(navigated.before_url.clone()),
            before_title: Some(navigated.before_title.clone()),
            ready_state: Some(navigated.ready_state.clone()),
            observed_at_unix_ms: None,
            active: Some(false),
            highlighted: None,
            pinned: None,
        });
        Ok(redact_cdp_navigate_tab_response_urls(
            CdpNavigateTabResponse {
                session_id: session_id.to_owned(),
                window_hwnd,
                transport: "chrome_tabs_extension".to_owned(),
                endpoint,
                cdp_target_id: navigated.target_id,
                action,
                requested_url: navigated.requested_url,
                before_url: navigated.before_url,
                before_title: navigated.before_title,
                after_url: navigated.after_url,
                after_title: navigated.after_title,
                ready_state: navigated.ready_state,
                history_current_index: navigated.history_current_index,
                history_entry_count: navigated.history_entry_count,
                history_readback_source: navigated.history_readback_source,
                readback_backend: navigated.readback_backend,
                navigation_error_text: navigated.navigation_error_text,
                is_download: navigated.is_download,
                download_status: navigated.download_status,
                download_id: navigated.download_id,
                download_url: navigated.download_url,
                download_final_url: navigated.download_final_url,
                download_filename: navigated.download_filename,
                download_state: navigated.download_state,
                download_match_reason: navigated.download_match_reason,
                backend_tier_used: "chrome_tabs".to_owned(),
                required_foreground: false,
                target_candidate_count: navigated.target_candidate_count,
                target_selection_reason: navigated.target_selection_reason,
            },
        ))
    }

    #[cfg(windows)]
    async fn cdp_activate_tab_impl(
        &self,
        session_id: &str,
        window_hwnd: i64,
        cdp_target_id: &str,
        wait_timeout_ms: u64,
    ) -> Result<CdpActivateTabResponse, ErrorData> {
        if let Some(endpoint) = synapse_a11y::endpoint_for_window(window_hwnd) {
            let activated = synapse_a11y::cdp_activate_target(&endpoint, cdp_target_id)
                .await
                .map_err(|error| {
                    mcp_error(
                        error.code(),
                        format!(
                            "cdp_activate_tab raw Target.activateTarget command/readback failed: {error}"
                        ),
                    )
                })?;
            tracing::info!(
                code = "CDP_BACKGROUND_TAB_ACTIVATED",
                session_id = %session_id,
                hwnd = window_hwnd,
                endpoint = %endpoint,
                cdp_target_id = %activated.target_id,
                transport = "raw_cdp",
                "readback=Target.activateTarget outcome=target_activated_without_foreground"
            );
            return Ok(CdpActivateTabResponse {
                session_id: session_id.to_owned(),
                window_hwnd,
                transport: "raw_cdp".to_owned(),
                endpoint,
                cdp_target_id: activated.target_id,
                before_active: None,
                active: true,
                url: redact_url_for_public_readback(&activated.url),
                title: activated.title,
                readback_backend: "Target.activateTarget".to_owned(),
                backend_tier_used: "cdp".to_owned(),
                required_foreground: false,
                target_candidate_count: 0,
                target_selection_reason: "target_id".to_owned(),
            });
        }

        let human_os_foreground_before_hwnd = current_human_os_foreground_hwnd();
        let activated = crate::chrome_debugger_bridge::activate_tab(
            window_hwnd,
            cdp_target_id,
            wait_timeout_ms,
        )
        .await
        .map_err(|error| {
            mcp_error(
                error.code(),
                format!(
                    "cdp_activate_tab Chrome bridge chrome.tabs.update({{active:true}}) failed: {}",
                    error.detail()
                ),
            )
        })?;
        let human_os_foreground_after_hwnd = current_human_os_foreground_hwnd();
        if background_tab_activation_foregrounded_requested_window(
            human_os_foreground_before_hwnd,
            human_os_foreground_after_hwnd,
            window_hwnd,
        ) {
            return Err(mcp_error(
                error_codes::ACTION_POSTCONDITION_FAILED,
                format!(
                    "cdp_activate_tab refused target {cdp_target_id:?}: Chrome bridge changed the human OS foreground from {human_os_foreground_before_hwnd:?} to requested HWND {window_hwnd:#x} while required_foreground=false"
                ),
            ));
        }
        let endpoint = activated
            .extension_id
            .as_deref()
            .map(chrome_debugger_endpoint)
            .unwrap_or_else(chrome_debugger_default_endpoint);
        tracing::info!(
            code = "CDP_BACKGROUND_TAB_ACTIVATED",
            session_id = %session_id,
            hwnd = window_hwnd,
            endpoint = %endpoint,
            cdp_target_id = %activated.target_id,
            tab_id = activated.tab_id,
            before_active = ?activated.before_active,
            active = activated.active,
            transport = "chrome_tabs_extension",
            "readback=chrome.tabs.get outcome=target_activated_without_foreground"
        );
        Ok(CdpActivateTabResponse {
            session_id: session_id.to_owned(),
            window_hwnd,
            transport: "chrome_tabs_extension".to_owned(),
            endpoint,
            cdp_target_id: activated.target_id,
            before_active: activated.before_active,
            active: activated.active,
            url: redact_url_for_public_readback(&activated.url),
            title: activated.title,
            readback_backend: activated.readback_backend,
            backend_tier_used: "chrome_tabs".to_owned(),
            required_foreground: false,
            target_candidate_count: activated.target_candidate_count,
            target_selection_reason: activated.target_selection_reason,
        })
    }

    #[cfg(not(windows))]
    async fn cdp_activate_tab_impl(
        &self,
        _session_id: &str,
        _window_hwnd: i64,
        _cdp_target_id: &str,
        _wait_timeout_ms: u64,
    ) -> Result<CdpActivateTabResponse, ErrorData> {
        Err(mcp_error(
            error_codes::A11Y_NOT_AVAILABLE,
            "cdp_activate_tab is only available on Windows in this build",
        ))
    }

    #[cfg(not(windows))]
    async fn cdp_close_tab_impl(
        &self,
        _session_id: &str,
        _cdp_target_id: &str,
        _owner_key: &str,
        _owner: CdpTargetOwner,
    ) -> Result<CdpCloseTabResponse, ErrorData> {
        Err(mcp_error(
            error_codes::A11Y_NOT_AVAILABLE,
            "cdp_close_tab is only available on Windows in this build",
        ))
    }

    #[cfg(not(windows))]
    async fn cdp_navigate_tab_impl(
        &self,
        _source_tool: &str,
        _session_id: &str,
        _window_hwnd: i64,
        _cdp_target_id: &str,
        _action: CdpNavigateAction,
        _requested_url: Option<&str>,
        _wait_timeout_ms: u64,
        _ignore_cache: bool,
    ) -> Result<CdpNavigateTabResponse, ErrorData> {
        Err(mcp_error(
            error_codes::A11Y_NOT_AVAILABLE,
            "cdp_navigate_tab is only available on Windows in this build",
        ))
    }
}

fn perception_window_hwnd(
    tool: &str,
    target: &Option<SessionTarget>,
    explicit_hwnd: Option<i64>,
) -> Result<Option<i64>, ErrorData> {
    if explicit_hwnd.is_some() {
        return Ok(explicit_hwnd);
    }
    match target {
        Some(SessionTarget::Window { hwnd }) => Ok(Some(*hwnd)),
        Some(SessionTarget::Cdp {
            window_hwnd,
            cdp_target_id,
        }) => Err(cdp_target_perception_error(
            tool,
            *window_hwnd,
            cdp_target_id,
        )),
        None => Ok(None),
    }
}

fn cdp_target_perception_error(tool: &str, window_hwnd: i64, cdp_target_id: &str) -> ErrorData {
    mcp_error(
        error_codes::TARGET_CDP_UNRESOLVED,
        format!(
            "{tool} cannot use session CDP target {cdp_target_id:?} in browser window {window_hwnd:#x} through the window/foreground perception path; refusing to downgrade the tab target to the browser HWND because that can read the human foreground tab. Use a true target-specific browser readback surface or pass an explicit window_hwnd intentionally."
        ),
    )
}

fn target_cdp_id(target: &Option<SessionTarget>) -> Option<String> {
    match target {
        Some(SessionTarget::Cdp { cdp_target_id, .. }) => Some(cdp_target_id.clone()),
        Some(SessionTarget::Window { .. }) | None => None,
    }
}

fn cdp_target_owner_key(window_hwnd: i64, endpoint: &str, cdp_target_id: &str) -> String {
    format!(
        "cdp:0x{window_hwnd:x}:{}:{}",
        endpoint.trim(),
        normalize_cdp_target_id(cdp_target_id)
    )
}

fn normalize_cdp_target_id(cdp_target_id: &str) -> String {
    cdp_target_id.trim().to_ascii_lowercase()
}

fn cdp_target_ids_equal(left: &str, right: &str) -> bool {
    normalize_cdp_target_id(left) == normalize_cdp_target_id(right)
}

fn session_targets_equal(left: &SessionTarget, right: &SessionTarget) -> bool {
    match (left, right) {
        (SessionTarget::Window { hwnd: left }, SessionTarget::Window { hwnd: right }) => {
            left == right
        }
        (
            SessionTarget::Cdp {
                window_hwnd: left_hwnd,
                cdp_target_id: left_id,
            },
            SessionTarget::Cdp {
                window_hwnd: right_hwnd,
                cdp_target_id: right_id,
            },
        ) => left_hwnd == right_hwnd && cdp_target_ids_equal(left_id, right_id),
        (SessionTarget::Window { .. }, SessionTarget::Cdp { .. })
        | (SessionTarget::Cdp { .. }, SessionTarget::Window { .. }) => false,
    }
}

fn select_cdp_owner_for_session(
    tool: &str,
    session_id: &str,
    target_id: &str,
    active_target: Option<&SessionTarget>,
    owners: Vec<(String, CdpTargetOwner)>,
) -> Result<(String, CdpTargetOwner), ErrorData> {
    if owners.len() == 1 {
        return owners.into_iter().next().ok_or_else(|| {
            mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                format!("{tool} internal owner selection lost single CDP owner"),
            )
        });
    }
    let active_window = match active_target {
        Some(SessionTarget::Cdp {
            window_hwnd,
            cdp_target_id,
        }) if cdp_target_ids_equal(cdp_target_id, target_id) => Some(*window_hwnd),
        Some(SessionTarget::Window { .. } | SessionTarget::Cdp { .. }) | None => None,
    };
    if let Some(active_window) = active_window {
        let active_matches = owners
            .iter()
            .filter(|(_key, owner)| owner.window_hwnd == active_window)
            .cloned()
            .collect::<Vec<_>>();
        if active_matches.len() == 1 {
            return active_matches.into_iter().next().ok_or_else(|| {
                mcp_error(
                    error_codes::TOOL_INTERNAL_ERROR,
                    format!("{tool} internal owner selection lost active CDP owner"),
                )
            });
        }
    }
    let owner_surfaces = owners
        .iter()
        .map(|(_key, owner)| format!("hwnd=0x{:x},endpoint={}", owner.window_hwnd, owner.endpoint))
        .collect::<Vec<_>>()
        .join(" | ");
    Err(mcp_error(
        error_codes::ACTION_TARGET_INVALID,
        format!(
            "{tool} refused target {target_id:?}: target id is ambiguous for MCP session {session_id:?}; set this session's active CDP target or pass a target id that maps to one owned browser surface. matches={owner_surfaces}"
        ),
    ))
}

fn select_persisted_cdp_owner_for_authorized_session(
    tool: &str,
    session_id: &str,
    target_id: &str,
    active_target: Option<&SessionTarget>,
    explicit_target_authority: Option<&SessionTarget>,
    owners: Vec<(String, PersistedCdpTargetOwner)>,
) -> Result<(String, PersistedCdpTargetOwner), ErrorData> {
    if owners.len() == 1 {
        return owners.into_iter().next().ok_or_else(|| {
            mcp_error(
                error_codes::TOOL_INTERNAL_ERROR,
                format!("{tool} internal persisted owner selection lost single CDP owner"),
            )
        });
    }
    let active_window = match active_target {
        Some(SessionTarget::Cdp {
            window_hwnd,
            cdp_target_id,
        }) if cdp_target_ids_equal(cdp_target_id, target_id) => Some(*window_hwnd),
        Some(SessionTarget::Window { .. } | SessionTarget::Cdp { .. }) | None => None,
    };
    if let Some(active_window) = active_window {
        let active_matches = owners
            .iter()
            .filter(|(_key, owner)| owner.owner.window_hwnd == active_window)
            .cloned()
            .collect::<Vec<_>>();
        if active_matches.len() == 1 {
            return active_matches.into_iter().next().ok_or_else(|| {
                mcp_error(
                    error_codes::TOOL_INTERNAL_ERROR,
                    format!("{tool} internal persisted owner selection lost active CDP owner"),
                )
            });
        }
    }
    if let Some(SessionTarget::Cdp {
        window_hwnd,
        cdp_target_id,
    }) = explicit_target_authority
        && cdp_target_ids_equal(cdp_target_id, target_id)
    {
        let explicit_matches = owners
            .iter()
            .filter(|(_key, owner)| owner.owner.window_hwnd == *window_hwnd)
            .cloned()
            .collect::<Vec<_>>();
        if explicit_matches.len() == 1 {
            return explicit_matches.into_iter().next().ok_or_else(|| {
                mcp_error(
                    error_codes::TOOL_INTERNAL_ERROR,
                    format!("{tool} internal persisted owner selection lost explicit CDP owner"),
                )
            });
        }
    }
    let owner_surfaces = owners
        .iter()
        .map(|(_key, row)| {
            format!(
                "hwnd=0x{:x},endpoint={},owner_session_id={}",
                row.owner.window_hwnd, row.owner.endpoint, row.owner_session_id
            )
        })
        .collect::<Vec<_>>()
        .join(" | ");
    Err(mcp_error(
        error_codes::ACTION_TARGET_INVALID,
        format!(
            "{tool} refused recovered target {target_id:?}: target id is ambiguous for MCP session {session_id:?}; set this session's active CDP target or explicit set_target request to the exact claimed browser surface. matches={owner_surfaces}"
        ),
    ))
}

fn cdp_close_unowned_error(
    target_id: &str,
    session_id: &str,
    owners: &[(String, CdpTargetOwner)],
) -> ErrorData {
    if owners.is_empty() {
        return mcp_error(
            error_codes::ACTION_TARGET_INVALID,
            format!(
                "cdp_close_tab refused target {target_id:?}: target is not owned by this session, has no persisted Synapse-created ownership row, or was already closed"
            ),
        );
    }
    let owner_sessions = owners
        .iter()
        .map(|(_key, owner)| owner.session_id.as_str())
        .collect::<Vec<_>>()
        .join(",");
    mcp_error(
        error_codes::ACTION_TARGET_INVALID,
        format!(
            "cdp_close_tab refused target {target_id:?}: owner_session_id(s)={owner_sessions:?}, requesting_session_id={session_id:?}",
        ),
    )
}

fn cdp_owner_session_mismatch_error(
    tool: &str,
    target_id: &str,
    session_id: &str,
    owners: &[(String, CdpTargetOwner)],
) -> ErrorData {
    let owner_sessions = owners
        .iter()
        .map(|(_key, owner)| owner.session_id.as_str())
        .collect::<Vec<_>>()
        .join(",");
    mcp_error(
        error_codes::ACTION_TARGET_INVALID,
        format!(
            "{tool} refused target {target_id:?}: owner_session_id(s)={owner_sessions:?}, requesting_session_id={session_id:?}",
        ),
    )
}

fn cdp_target_info_resolution_request_details(
    session_id: &str,
    params: &CdpTargetInfoParams,
) -> Value {
    json!({
        "session_id": session_id,
        "window_hwnd": params.window_hwnd,
        "requested_cdp_target": cdp_target_id_audit_ref(params.cdp_target_id.as_deref()),
        "required_foreground": false,
        "phase": "target_resolution",
    })
}

fn browser_evaluate_resolution_request_details(
    session_id: &str,
    params: &BrowserEvaluateParams,
) -> Value {
    json!({
        "session_id": session_id,
        "window_hwnd": params.window_hwnd,
        "requested_cdp_target": cdp_target_id_audit_ref(params.cdp_target_id.as_deref()),
        "expression_len": params.expression.len(),
        "await_promise": params.await_promise.unwrap_or(true),
        "return_by_value": params.return_by_value.unwrap_or(true),
        "required_foreground": false,
        "phase": "target_resolution",
    })
}

fn cdp_navigate_resolution_request_details(
    session_id: &str,
    params: &CdpNavigateTabParams,
    requested_url: Option<&str>,
    wait_timeout_ms: u64,
) -> Value {
    json!({
        "session_id": session_id,
        "window_hwnd": params.window_hwnd,
        "requested_cdp_target": cdp_target_id_audit_ref(params.cdp_target_id.as_deref()),
        "action": params.action,
        "requested_url": requested_url,
        "wait_timeout_ms": wait_timeout_ms,
        "ignore_cache": params.ignore_cache.unwrap_or(false),
        "required_foreground": false,
        "phase": "target_resolution",
    })
}

fn cdp_activate_resolution_request_details(
    session_id: &str,
    params: &CdpActivateTabParams,
    wait_timeout_ms: u64,
) -> Value {
    json!({
        "session_id": session_id,
        "window_hwnd": params.window_hwnd,
        "requested_cdp_target": cdp_target_id_audit_ref(params.cdp_target_id.as_deref()),
        "wait_timeout_ms": wait_timeout_ms,
        "required_foreground": false,
        "phase": "target_resolution",
    })
}

pub(super) fn cdp_target_id_audit_ref(target_id: Option<&str>) -> Value {
    match target_id {
        Some(target_id) => json!({
            "present": true,
            "len": target_id.chars().count(),
            "sha256": sha256_hex(target_id.as_bytes()),
        }),
        None => json!({
            "present": false,
        }),
    }
}

fn browser_tab_entry(
    window_hwnd: i64,
    tab: crate::chrome_debugger_bridge::ChromeDebuggerTabTarget,
) -> BrowserTabEntry {
    let url = tab.url;
    BrowserTabEntry {
        target: TargetWire::Cdp {
            window_hwnd,
            cdp_target_id: tab.target_id.clone(),
        },
        window_hwnd,
        cdp_target_id: tab.target_id,
        tab_id: tab.tab_id,
        chrome_window_id: tab.chrome_window_id,
        index: tab.index,
        target_type: tab.target_type,
        title: redact_title_for_public_url_readback(&url, tab.title),
        url: redact_url_for_public_readback(&url),
        ready_state: tab.ready_state,
        active: tab.active,
        highlighted: tab.highlighted,
        pinned: tab.pinned,
        target_attached: tab.target_attached,
    }
}

fn redact_browser_tabs_mutation_urls(mut mutation: BrowserTabsMutation) -> BrowserTabsMutation {
    mutation.requested_url = mutation
        .requested_url
        .map(|url| redact_url_for_public_readback(&url));
    mutation.selected_tab = mutation.selected_tab.map(redact_browser_tab_entry_url);
    mutation.activated_tab = mutation.activated_tab.map(redact_browser_tab_entry_url);
    mutation
}

fn redact_browser_tab_entry_url(mut entry: BrowserTabEntry) -> BrowserTabEntry {
    let url = entry.url;
    entry.title = redact_title_for_public_url_readback(&url, entry.title);
    entry.url = redact_url_for_public_readback(&url);
    entry
}

fn redact_browser_navigation_event(mut event: BrowserNavigationEvent) -> BrowserNavigationEvent {
    event.url = redact_url_for_public_readback(&event.url);
    event.requested_url = redact_url_opt_for_public_readback(event.requested_url);
    event.before_url = redact_url_opt_for_public_readback(event.before_url);
    event
}

fn redact_cdp_navigate_tab_response_urls(
    mut response: CdpNavigateTabResponse,
) -> CdpNavigateTabResponse {
    response.requested_url = redact_url_opt_for_public_readback(response.requested_url);
    response.before_url = redact_url_for_public_readback(&response.before_url);
    response.after_url = redact_url_for_public_readback(&response.after_url);
    response.download_url = redact_url_opt_for_public_readback(response.download_url);
    response.download_final_url = redact_url_opt_for_public_readback(response.download_final_url);
    response
}

fn redact_browser_wait_response_urls(mut response: BrowserWaitResponse) -> BrowserWaitResponse {
    response.text = response.text.map(|mut value| {
        value.url = redact_url_for_public_readback(&value.url);
        value
    });
    response.load_state = response.load_state.map(|mut value| {
        value.url = redact_url_for_public_readback(&value.url);
        value
    });
    response.url = response.url.map(|mut value| {
        value.url_pattern = redact_url_for_public_readback(&value.url_pattern);
        value.url = redact_url_for_public_readback(&value.url);
        value
    });
    response.selector = response.selector.map(|mut value| {
        value.frame = value.frame.map(redact_browser_located_frame_urls);
        value.url = redact_url_for_public_readback(&value.url);
        value
    });
    response.function = response.function.map(|mut value| {
        value.url = redact_url_for_public_readback(&value.url);
        value
    });
    response.request = response.request.map(|mut value| {
        value.url_pattern = redact_url_opt_for_public_readback(value.url_pattern);
        value.matched_entry = redact_browser_network_wait_entry_urls(value.matched_entry);
        value
    });
    response.response = response.response.map(|mut value| {
        value.url_pattern = redact_url_opt_for_public_readback(value.url_pattern);
        value.matched_entry = redact_browser_network_wait_entry_urls(value.matched_entry);
        value
    });
    response
}

fn redact_browser_network_wait_entry_urls(
    mut entry: BrowserNetworkWaitEntry,
) -> BrowserNetworkWaitEntry {
    entry.url = redact_url_opt_for_public_readback(entry.url);
    entry.response_url = redact_url_opt_for_public_readback(entry.response_url);
    entry
}

fn redact_browser_located_frame_urls(mut frame: BrowserLocatedFrame) -> BrowserLocatedFrame {
    frame.url = redact_url_opt_for_public_readback(frame.url);
    frame
}

fn select_single_active_browser_tab(
    tabs: &BrowserTabsResponse,
) -> Result<&BrowserTabEntry, ErrorData> {
    let active_tabs = tabs
        .tabs
        .iter()
        .filter(|tab| tab.active)
        .collect::<Vec<_>>();
    match active_tabs.as_slice() {
        [active] => Ok(*active),
        [] => Err(mcp_error(
            error_codes::ACTION_TARGET_INVALID,
            format!(
                "browser_adopt_active_tab found no active Chrome tab for window {:#x}; target_count={}",
                tabs.window_hwnd, tabs.target_count
            ),
        )),
        many => Err(mcp_error(
            error_codes::ACTION_TARGET_INVALID,
            format!(
                "browser_adopt_active_tab refused ambiguous active tab state for window {:#x}: active_tab_count={} target_count={}",
                tabs.window_hwnd,
                many.len(),
                tabs.target_count
            ),
        )),
    }
}

fn target_wire(target: &SessionTarget) -> TargetWire {
    match target {
        SessionTarget::Window { hwnd } => TargetWire::Window { window_hwnd: *hwnd },
        SessionTarget::Cdp {
            window_hwnd,
            cdp_target_id,
        } => TargetWire::Cdp {
            window_hwnd: *window_hwnd,
            cdp_target_id: cdp_target_id.clone(),
        },
    }
}

#[cfg(windows)]
fn raw_cdp_navigation_action(action: CdpNavigateAction) -> synapse_a11y::CdpPageNavigationAction {
    match action {
        CdpNavigateAction::Navigate => synapse_a11y::CdpPageNavigationAction::Navigate,
        CdpNavigateAction::Reload => synapse_a11y::CdpPageNavigationAction::Reload,
        CdpNavigateAction::Back => synapse_a11y::CdpPageNavigationAction::Back,
        CdpNavigateAction::Forward => synapse_a11y::CdpPageNavigationAction::Forward,
    }
}

fn cdp_navigate_action_wire(action: CdpNavigateAction) -> &'static str {
    match action {
        CdpNavigateAction::Navigate => "navigate",
        CdpNavigateAction::Reload => "reload",
        CdpNavigateAction::Back => "back",
        CdpNavigateAction::Forward => "forward",
    }
}

pub(super) fn chrome_debugger_default_endpoint() -> String {
    chrome_debugger_endpoint("leoocgnkjnplbfdbklajepahofecgfbk")
}

pub(super) fn chrome_debugger_endpoint(extension_id: &str) -> String {
    format!("chrome-extension://{extension_id}/chrome.tabs")
}

pub(super) fn validate_cdp_target_id(cdp_target_id: &str) -> Result<(), ErrorData> {
    if cdp_target_id.trim().is_empty() {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "cdp_target_id must not be empty",
        ));
    }
    if cdp_target_id.chars().count() > 512 {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "cdp_target_id must be at most 512 Unicode scalar values",
        ));
    }
    if cdp_target_id.contains('\0') {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "cdp_target_id must not contain NUL",
        ));
    }
    Ok(())
}

fn validate_browser_tabs_params(params: BrowserTabsParams) -> Result<BrowserTabsParams, ErrorData> {
    if let Some(cdp_target_id) = params.cdp_target_id.as_deref() {
        validate_cdp_target_id(cdp_target_id)?;
    }
    if let Some(url) = params.url.as_deref() {
        validate_cdp_tab_url(url)?;
    }
    match params.operation {
        BrowserTabsOperation::List => {
            if params.cdp_target_id.is_some() {
                return Err(mcp_error(
                    error_codes::TOOL_PARAMS_INVALID,
                    "browser_tabs operation=list does not accept cdp_target_id",
                ));
            }
            if params.url.is_some() {
                return Err(mcp_error(
                    error_codes::TOOL_PARAMS_INVALID,
                    "browser_tabs operation=list does not accept url",
                ));
            }
        }
        BrowserTabsOperation::Select | BrowserTabsOperation::Activate => {
            if params.cdp_target_id.is_none() {
                return Err(mcp_error(
                    error_codes::TOOL_PARAMS_INVALID,
                    format!(
                        "browser_tabs operation={} requires cdp_target_id",
                        match params.operation {
                            BrowserTabsOperation::Select => "select",
                            BrowserTabsOperation::Activate => "activate",
                            _ => unreachable!("operation arm is select or activate"),
                        }
                    ),
                ));
            }
            if params.url.is_some() {
                return Err(mcp_error(
                    error_codes::TOOL_PARAMS_INVALID,
                    format!(
                        "browser_tabs operation={} does not accept url",
                        match params.operation {
                            BrowserTabsOperation::Select => "select",
                            BrowserTabsOperation::Activate => "activate",
                            _ => unreachable!("operation arm is select or activate"),
                        }
                    ),
                ));
            }
        }
        BrowserTabsOperation::New => {
            if params.url.is_none() {
                return Err(mcp_error(
                    error_codes::TOOL_PARAMS_INVALID,
                    "browser_tabs operation=new requires url; pass an empty string for about:blank",
                ));
            }
            if params.cdp_target_id.is_some() {
                return Err(mcp_error(
                    error_codes::TOOL_PARAMS_INVALID,
                    "browser_tabs operation=new does not accept cdp_target_id",
                ));
            }
        }
        BrowserTabsOperation::Close => {
            if params.cdp_target_id.is_none() {
                return Err(mcp_error(
                    error_codes::TOOL_PARAMS_INVALID,
                    "browser_tabs operation=close requires cdp_target_id",
                ));
            }
            if params.url.is_some() {
                return Err(mcp_error(
                    error_codes::TOOL_PARAMS_INVALID,
                    "browser_tabs operation=close does not accept url",
                ));
            }
        }
    }
    Ok(params)
}

#[derive(Debug)]
struct BrowserDownloadsValidation {
    params: BrowserDownloadsParams,
    output_path: Option<PathBuf>,
}

fn validate_browser_downloads_params(
    mut params: BrowserDownloadsParams,
) -> Result<BrowserDownloadsValidation, ErrorData> {
    if let Some(download_id) = params.download_id {
        if download_id < 0 {
            return Err(mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                format!("browser_downloads download_id must be non-negative; got {download_id}"),
            ));
        }
    }
    validate_browser_downloads_optional_string(&params.url_contains, "url_contains", 2048)?;
    validate_browser_downloads_optional_string(
        &params.filename_contains,
        "filename_contains",
        1024,
    )?;
    validate_browser_downloads_optional_string(&params.mime_contains, "mime_contains", 256)?;
    if let Some(state) = params.state.as_deref() {
        validate_browser_download_state(state)?;
    }
    if let Some(limit) = params.limit {
        if !(1..=500).contains(&limit) {
            return Err(mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                format!("browser_downloads limit must be in 1..=500; got {limit}"),
            ));
        }
    }
    if let Some(wait_timeout_ms) = params.wait_timeout_ms {
        if !(1..=300_000).contains(&wait_timeout_ms) {
            return Err(mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                format!(
                    "browser_downloads wait_timeout_ms must be in 1..=300000; got {wait_timeout_ms}"
                ),
            ));
        }
    }
    let output_path = match params.operation {
        BrowserDownloadsOperation::List | BrowserDownloadsOperation::Wait => {
            if params.path.is_some() {
                return Err(mcp_error(
                    error_codes::TOOL_PARAMS_INVALID,
                    "browser_downloads operation=list/wait does not accept path",
                ));
            }
            None
        }
        BrowserDownloadsOperation::Save | BrowserDownloadsOperation::Move => {
            let Some(path) = params.path.as_deref() else {
                return Err(mcp_error(
                    error_codes::TOOL_PARAMS_INVALID,
                    "browser_downloads operation=save/move requires path",
                ));
            };
            if let Some(state) = params.state.as_deref() {
                if state != "complete" {
                    return Err(mcp_error(
                        error_codes::TOOL_PARAMS_INVALID,
                        "browser_downloads operation=save/move requires state omitted or complete",
                    ));
                }
            }
            params.state = Some("complete".to_owned());
            let output_path = browser_download_output_path(path)?;
            ensure_download_output_path_available(&output_path, params.overwrite)?;
            Some(output_path)
        }
    };
    Ok(BrowserDownloadsValidation {
        params,
        output_path,
    })
}

fn validate_browser_downloads_optional_string(
    value: &Option<String>,
    field_name: &str,
    max_chars: usize,
) -> Result<(), ErrorData> {
    if let Some(value) = value.as_deref() {
        if value.contains('\0') {
            return Err(mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                format!("browser_downloads {field_name} must not contain NUL"),
            ));
        }
        if value.chars().count() > max_chars {
            return Err(mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                format!("browser_downloads {field_name} must be at most {max_chars} characters"),
            ));
        }
    }
    Ok(())
}

fn validate_browser_download_state(state: &str) -> Result<(), ErrorData> {
    if matches!(state, "in_progress" | "interrupted" | "complete") {
        return Ok(());
    }
    Err(mcp_error(
        error_codes::TOOL_PARAMS_INVALID,
        format!(
            "browser_downloads state must be one of in_progress, interrupted, or complete; got {state:?}"
        ),
    ))
}

fn browser_downloads_bridge_payload(params: &BrowserDownloadsParams) -> Value {
    json!({
        "operation": browser_downloads_operation_wire(params.operation),
        "downloadId": params.download_id,
        "urlContains": params.url_contains,
        "filenameContains": params.filename_contains,
        "mimeContains": params.mime_contains,
        "state": params.state,
        "sinceUnixMs": params.since_unix_ms,
        "sinceEventSeq": params.since_event_seq,
        "limit": params.limit,
        "waitTimeoutMs": params.wait_timeout_ms,
    })
}

fn browser_downloads_operation_wire(operation: BrowserDownloadsOperation) -> &'static str {
    match operation {
        BrowserDownloadsOperation::List => "list",
        BrowserDownloadsOperation::Wait => "wait",
        BrowserDownloadsOperation::Save => "save",
        BrowserDownloadsOperation::Move => "move",
    }
}

fn browser_download_entry_from_bridge(
    entry: crate::chrome_debugger_bridge::ChromeDebuggerDownloadEntry,
) -> BrowserDownloadEntry {
    BrowserDownloadEntry {
        id: entry.id,
        url: redact_url_for_public_readback(&entry.url),
        final_url: redact_url_for_public_readback(&entry.final_url),
        filename: entry.filename,
        filename_basename: entry.filename_basename,
        mime: entry.mime,
        start_time: entry.start_time,
        end_time: entry.end_time,
        estimated_end_time: entry.estimated_end_time,
        state: entry.state,
        paused: entry.paused,
        can_resume: entry.can_resume,
        danger: entry.danger,
        error: entry.error,
        bytes_received: entry.bytes_received,
        total_bytes: entry.total_bytes,
        file_size: entry.file_size,
        exists: entry.exists,
        incognito: entry.incognito,
        referrer: redact_url_for_public_readback(&entry.referrer),
    }
}

fn browser_download_event_from_bridge(
    event: crate::chrome_debugger_bridge::ChromeDebuggerDownloadEvent,
) -> BrowserDownloadEvent {
    BrowserDownloadEvent {
        seq: event.seq,
        event_kind: event.event_kind,
        timestamp_unix_ms: event.timestamp_unix_ms,
        download_id: event.download_id,
        url: redact_url_for_public_readback(&event.url),
        final_url: redact_url_for_public_readback(&event.final_url),
        filename: event.filename,
        filename_basename: event.filename_basename,
        state: event.state,
        danger: event.danger,
        error: event.error,
        bytes_received: event.bytes_received,
        total_bytes: event.total_bytes,
        file_size: event.file_size,
        delta: event.delta,
    }
}

/// Default and ceiling on `browser_console_messages` entries returned per call.
const DEFAULT_BROWSER_CONSOLE_MESSAGES: usize = 200;
const MAX_BROWSER_CONSOLE_MESSAGES: usize = 5_000;
const DEFAULT_BROWSER_BINDING_CALLS: usize = 200;
const MAX_BROWSER_BINDING_CALLS: usize = 5_000;
const BROWSER_BINDING_NAME_MAX_CHARS: usize = 128;

/// Projects a captured [`synapse_a11y::ConsoleEntry`] into the MCP response shape.
#[cfg(windows)]
fn console_message_from_entry(entry: synapse_a11y::ConsoleEntry) -> ConsoleMessage {
    ConsoleMessage {
        seq: entry.seq,
        source: entry.source.to_owned(),
        level: entry.level,
        text: entry.text,
        args: entry.args,
        url: redact_url_opt_for_public_readback(entry.url),
        line: entry.line,
        column: entry.column,
        stack: entry.stack,
        category: entry.category,
        timestamp_ms: entry.timestamp_ms,
    }
}

#[cfg(windows)]
fn browser_binding_call_from_entry(
    entry: synapse_a11y::CdpBindingCall,
) -> super::BrowserBindingCall {
    super::BrowserBindingCall {
        seq: entry.seq,
        name: entry.name,
        payload: entry.payload,
        payload_len: entry.payload_len,
        payload_truncated: entry.payload_truncated,
        payload_json: entry.payload_json,
        execution_context_id: entry.execution_context_id,
        timestamp_ms: entry.timestamp_ms,
    }
}

#[cfg(windows)]
fn browser_binding_call_from_bridge(
    entry: crate::chrome_debugger_bridge::ChromeDebuggerBindingCall,
) -> super::BrowserBindingCall {
    super::BrowserBindingCall {
        seq: entry.seq,
        name: entry.name,
        payload: entry.payload,
        payload_len: entry.payload_len,
        payload_truncated: entry.payload_truncated,
        payload_json: entry.payload_json,
        execution_context_id: entry.execution_context_id,
        timestamp_ms: entry.timestamp_ms,
    }
}

/// Upper bound on the evaluated expression size. Generous enough for injected
/// helper bundles, but bounded so a single tool call cannot ship an unbounded
/// payload through the protocol.
const BROWSER_EVALUATE_MAX_EXPRESSION_BYTES: usize = 1_048_576;
const BROWSER_INIT_SCRIPT_MAX_SOURCE_BYTES: usize = BROWSER_EVALUATE_MAX_EXPRESSION_BYTES;
const BROWSER_INIT_SCRIPT_MAX_IDENTIFIER_CHARS: usize = 512;
const BROWSER_INIT_SCRIPT_MAX_WORLD_NAME_CHARS: usize = 256;
const BROWSER_TAG_MAX_CONTENT_BYTES: usize = BROWSER_EVALUATE_MAX_EXPRESSION_BYTES - (16 * 1024);
const BROWSER_TAG_MAX_URL_CHARS: usize = 8192;
const BROWSER_TAG_MAX_PATH_CHARS: usize = 4096;
const BROWSER_TAG_MAX_SCRIPT_TYPE_CHARS: usize = 128;

#[derive(Clone, Copy, Debug)]
enum BrowserTagKind {
    Script,
    Style,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BrowserTagSourceKind {
    Url,
    Content,
    Path,
}

fn browser_init_script_operation_name(operation: BrowserInitScriptOperation) -> &'static str {
    match operation {
        BrowserInitScriptOperation::Add => "add",
        BrowserInitScriptOperation::Remove => "remove",
    }
}

fn browser_expose_binding_operation_name(operation: BrowserExposeBindingOperation) -> &'static str {
    match operation {
        BrowserExposeBindingOperation::Add => "add",
        BrowserExposeBindingOperation::Read => "read",
        BrowserExposeBindingOperation::Remove => "remove",
    }
}

impl BrowserTagSourceKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Url => "url",
            Self::Content => "content",
            Self::Path => "path",
        }
    }
}

#[derive(Debug)]
struct ResolvedBrowserTagSource {
    kind: BrowserTagSourceKind,
    requested_url: Option<String>,
    path: Option<String>,
    content: String,
    content_len: usize,
}

#[cfg(windows)]
#[derive(Deserialize)]
struct BrowserAddTagPayload {
    tag_name: String,
    source_kind: String,
    requested_url: Option<String>,
    resolved_url: Option<String>,
    content_len: usize,
    element_marker: String,
}

const DEFAULT_BROWSER_WAIT_TIMEOUT_MS: u64 = 30_000;
const MAX_BROWSER_WAIT_TIMEOUT_MS: u64 = 30_000;
const DEFAULT_BROWSER_WAIT_POLLING_INTERVAL_MS: u64 = 100;
const MIN_BROWSER_WAIT_POLLING_INTERVAL_MS: u64 = 10;
const MAX_BROWSER_WAIT_POLLING_INTERVAL_MS: u64 = 5_000;
const BROWSER_WAIT_MAX_TEXT_BYTES: usize = 64 * 1024;
const BROWSER_WAIT_MAX_URL_PATTERN_CHARS: usize = 8192;
const BROWSER_WAIT_MAX_NETWORK_TOKEN_CHARS: usize = 128;

#[derive(Debug)]
struct NormalizedBrowserWaitForParams {
    state: BrowserWaitForState,
    text: Option<String>,
    timeout_ms: u64,
    polling_interval_ms: u64,
}

#[derive(Debug)]
struct NormalizedBrowserWaitForLoadStateParams {
    state: BrowserWaitForLoadStateState,
    timeout_ms: u64,
}

#[derive(Debug)]
struct NormalizedBrowserWaitForUrlParams {
    url: String,
    match_kind: BrowserWaitForUrlMatchKind,
    timeout_ms: u64,
    polling_interval_ms: u64,
}

#[derive(Debug)]
struct NormalizedBrowserNetworkWaitParams {
    url: Option<String>,
    match_kind: BrowserWaitForUrlMatchKind,
    url_regex: Option<regex::Regex>,
    method: Option<String>,
    status: Option<i64>,
    resource_type: Option<String>,
    timeout_ms: u64,
    polling_interval_ms: u64,
}

#[derive(Debug)]
struct NormalizedBrowserWaitForSelectorParams {
    locate: BrowserLocateParams,
    state: BrowserWaitForSelectorState,
    timeout_ms: u64,
    polling_interval_ms: u64,
    limit: usize,
}

#[derive(Clone, Debug, Default)]
struct BrowserWaitForSelectorObservation {
    returned_backend_node_ids: Vec<i64>,
    visible_backend_node_ids: Vec<i64>,
    hidden_backend_node_ids: Vec<i64>,
    truncated: bool,
}

#[cfg(windows)]
#[derive(Deserialize)]
struct BrowserWaitForPayload {
    condition_met: bool,
    timed_out: bool,
    elapsed_ms: u64,
    poll_count: u64,
    observed_text_len: usize,
}

#[derive(Debug)]
struct NormalizedBrowserWaitForFunctionParams {
    expression: String,
    args: Vec<Value>,
    timeout_ms: u64,
    polling_interval_ms: u64,
}

#[cfg(windows)]
#[derive(Deserialize)]
struct BrowserWaitForFunctionPayload {
    condition_met: bool,
    timed_out: bool,
    elapsed_ms: u64,
    poll_count: u64,
    value: Value,
    value_type: String,
    value_description: Option<String>,
    unserializable_value: Option<String>,
}

#[cfg(windows)]
#[derive(Debug)]
struct BrowserWaitForSelectorPoll {
    condition_met: bool,
    cdp_target_id: String,
    engine: String,
    query: String,
    match_count: usize,
    returned_count: usize,
    visible_count: usize,
    truncated: bool,
    element_id: Option<String>,
    frame: Option<BrowserLocatedFrame>,
    url: String,
    title: String,
}

#[cfg(windows)]
#[derive(Deserialize)]
struct BrowserWaitForSelectorElementState {
    is_connected: bool,
    is_visible: bool,
}

fn validate_browser_evaluate_params(params: &BrowserEvaluateParams) -> Result<(), ErrorData> {
    if params.expression.trim().is_empty() {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "browser_evaluate requires a non-empty expression",
        ));
    }
    if params.expression.len() > BROWSER_EVALUATE_MAX_EXPRESSION_BYTES {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "browser_evaluate expression is {} bytes; the maximum is {BROWSER_EVALUATE_MAX_EXPRESSION_BYTES} bytes",
                params.expression.len()
            ),
        ));
    }
    if let Some(target_id) = params.cdp_target_id.as_deref() {
        validate_cdp_target_id(target_id)?;
    }
    if let Some(args) = params.args.as_ref()
        && args.len() > BROWSER_EVALUATE_MAX_ARGS
    {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "browser_evaluate accepts at most {BROWSER_EVALUATE_MAX_ARGS} args; got {}",
                args.len()
            ),
        ));
    }
    Ok(())
}

fn validate_browser_wait_facade_params(
    params: BrowserWaitFacadeParams,
) -> Result<BrowserWaitParams, ErrorData> {
    match params.operation {
        BrowserWaitOperation::ForCondition => {
            let wait = params.wait.ok_or_else(|| {
                browser_wait_facade_error(
                    BrowserWaitOperation::ForCondition,
                    "missing_wait_spec",
                    "browser_wait operation=for_condition requires the `wait` spec object",
                    "send wait={condition,...} with exactly one nested condition spec",
                )
            })?;
            validate_browser_wait_condition_shape(&wait)?;
            Ok(wait)
        }
    }
}

fn validate_browser_wait_condition_shape(params: &BrowserWaitParams) -> Result<(), ErrorData> {
    let fields = [
        ("text", params.text.is_some()),
        ("load_state", params.load_state.is_some()),
        ("url", params.url.is_some()),
        ("selector", params.selector.is_some()),
        ("function", params.function.is_some()),
        ("request", params.request.is_some()),
        ("response", params.response.is_some()),
    ];
    let supplied = fields
        .iter()
        .filter_map(|(field, present)| present.then_some(*field))
        .collect::<Vec<_>>();
    let expected = match params.condition {
        BrowserWaitConditionKind::Text => "text",
        BrowserWaitConditionKind::LoadState => "load_state",
        BrowserWaitConditionKind::Url => "url",
        BrowserWaitConditionKind::Selector => "selector",
        BrowserWaitConditionKind::Function => "function",
        BrowserWaitConditionKind::Request => "request",
        BrowserWaitConditionKind::Response => "response",
    };
    if supplied.len() != 1 || supplied[0] != expected {
        return Err(browser_wait_facade_error(
            BrowserWaitOperation::ForCondition,
            browser_wait_condition_source_id(params),
            format!(
                "browser_wait condition={:?} requires exactly `{expected}` spec and no other condition specs; supplied={supplied:?}",
                params.condition
            ),
            "send exactly one nested condition spec whose field name matches condition",
        ));
    }
    Ok(())
}

fn browser_wait_facade_source_id(params: &BrowserWaitFacadeParams) -> String {
    match params.operation {
        BrowserWaitOperation::ForCondition => params
            .wait
            .as_ref()
            .map(browser_wait_condition_source_id)
            .unwrap_or_else(|| "missing_wait_spec".to_owned()),
    }
}

fn browser_wait_condition_source_id(params: &BrowserWaitParams) -> String {
    match params.condition {
        BrowserWaitConditionKind::Text => params
            .text
            .as_ref()
            .map(|spec| browser_wait_target_source(spec.window_hwnd, spec.cdp_target_id.as_deref()))
            .unwrap_or_else(|| "missing_text_spec".to_owned()),
        BrowserWaitConditionKind::LoadState => params
            .load_state
            .as_ref()
            .map(|spec| browser_wait_target_source(spec.window_hwnd, spec.cdp_target_id.as_deref()))
            .unwrap_or_else(|| "missing_load_state_spec".to_owned()),
        BrowserWaitConditionKind::Url => params
            .url
            .as_ref()
            .map(|spec| {
                format!(
                    "{};url_len={}",
                    browser_wait_target_source(spec.window_hwnd, spec.cdp_target_id.as_deref()),
                    spec.url.len()
                )
            })
            .unwrap_or_else(|| "missing_url_spec".to_owned()),
        BrowserWaitConditionKind::Selector => params
            .selector
            .as_ref()
            .map(|spec| {
                format!(
                    "{};query_len={}",
                    browser_wait_target_source(spec.window_hwnd, spec.cdp_target_id.as_deref()),
                    spec.query.len()
                )
            })
            .unwrap_or_else(|| "missing_selector_spec".to_owned()),
        BrowserWaitConditionKind::Function => params
            .function
            .as_ref()
            .map(|spec| {
                format!(
                    "{};expression_len={}",
                    browser_wait_target_source(spec.window_hwnd, spec.cdp_target_id.as_deref()),
                    spec.expression.len()
                )
            })
            .unwrap_or_else(|| "missing_function_spec".to_owned()),
        BrowserWaitConditionKind::Request => params
            .request
            .as_ref()
            .map(|spec| {
                format!(
                    "{};url_len={:?};method={:?}",
                    browser_wait_target_source(spec.window_hwnd, spec.cdp_target_id.as_deref()),
                    spec.url.as_ref().map(String::len),
                    spec.method
                )
            })
            .unwrap_or_else(|| "missing_request_spec".to_owned()),
        BrowserWaitConditionKind::Response => params
            .response
            .as_ref()
            .map(|spec| {
                format!(
                    "{};url_len={:?};status={:?}",
                    browser_wait_target_source(spec.window_hwnd, spec.cdp_target_id.as_deref()),
                    spec.url.as_ref().map(String::len),
                    spec.status
                )
            })
            .unwrap_or_else(|| "missing_response_spec".to_owned()),
    }
}

fn browser_wait_target_source(window_hwnd: Option<i64>, cdp_target_id: Option<&str>) -> String {
    match (window_hwnd, cdp_target_id) {
        (Some(hwnd), Some(target)) => format!("window_hwnd={hwnd:#x};cdp_target_id={target}"),
        (Some(hwnd), None) => format!("window_hwnd={hwnd:#x}"),
        (None, Some(target)) => format!("cdp_target_id={target}"),
        (None, None) => "active_session_target".to_owned(),
    }
}

fn browser_wait_facade_error(
    operation: BrowserWaitOperation,
    source_id: impl Into<String>,
    message: impl Into<String>,
    remediation: &'static str,
) -> ErrorData {
    let message = message.into();
    ErrorData::new(
        ErrorCode(-32099),
        message,
        Some(json!({
            "code": error_codes::TOOL_PARAMS_INVALID,
            "operation": operation.as_str(),
            "source_of_truth": BROWSER_WAIT_FACADE_SOURCE_OF_TRUTH,
            "source_id": source_id.into(),
            "readback_source_of_truth": BROWSER_WAIT_FACADE_READBACK_SOURCE_OF_TRUTH,
            "remediation": remediation,
        })),
    )
}

fn browser_wait_facade_delegate_error(
    operation: BrowserWaitOperation,
    source_id: impl Into<String>,
    error: ErrorData,
    remediation: &'static str,
) -> ErrorData {
    let cause_code = error
        .data
        .as_ref()
        .and_then(|data| data.get("code"))
        .and_then(Value::as_str)
        .unwrap_or(error_codes::TOOL_INTERNAL_ERROR)
        .to_owned();
    let cause = error.data.clone().unwrap_or(Value::Null);
    ErrorData::new(
        error.code,
        error.message.to_string(),
        Some(json!({
            "code": cause_code,
            "operation": operation.as_str(),
            "source_of_truth": BROWSER_WAIT_FACADE_SOURCE_OF_TRUTH,
            "source_id": source_id.into(),
            "readback_source_of_truth": BROWSER_WAIT_FACADE_READBACK_SOURCE_OF_TRUTH,
            "remediation": remediation,
            "cause": cause,
        })),
    )
}

fn validate_browser_wait_for_load_state_params(
    params: &BrowserWaitForLoadStateParams,
) -> Result<NormalizedBrowserWaitForLoadStateParams, ErrorData> {
    if let Some(target_id) = params.cdp_target_id.as_deref() {
        validate_cdp_target_id(target_id)?;
    }
    let timeout_ms =
        validate_browser_wait_timeout("browser_wait_for_load_state", params.timeout_ms)?;
    Ok(NormalizedBrowserWaitForLoadStateParams {
        state: params.state.unwrap_or_default(),
        timeout_ms,
    })
}

fn validate_browser_wait_for_url_params(
    params: &BrowserWaitForUrlParams,
) -> Result<NormalizedBrowserWaitForUrlParams, ErrorData> {
    if let Some(target_id) = params.cdp_target_id.as_deref() {
        validate_cdp_target_id(target_id)?;
    }
    let url = params.url.clone();
    if url.trim().is_empty() {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "browser_wait_for_url url must not be empty",
        ));
    }
    if url.chars().count() > BROWSER_WAIT_MAX_URL_PATTERN_CHARS {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "browser_wait_for_url url must be at most {BROWSER_WAIT_MAX_URL_PATTERN_CHARS} Unicode scalar values"
            ),
        ));
    }
    if url.contains('\0') {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "browser_wait_for_url url must not contain NUL",
        ));
    }
    if url.trim() != url {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "browser_wait_for_url url must not contain leading or trailing whitespace",
        ));
    }
    let match_kind = params.match_kind.unwrap_or_default();
    validate_browser_wait_for_url_pattern(match_kind, &url)?;
    let timeout_ms = validate_browser_wait_timeout("browser_wait_for_url", params.timeout_ms)?;
    let polling_interval_ms =
        validate_browser_wait_polling_interval("browser_wait_for_url", params.polling_interval_ms)?;
    Ok(NormalizedBrowserWaitForUrlParams {
        url,
        match_kind,
        timeout_ms,
        polling_interval_ms,
    })
}

fn validate_browser_wait_for_request_params(
    params: &BrowserWaitForRequestParams,
) -> Result<NormalizedBrowserNetworkWaitParams, ErrorData> {
    validate_browser_network_wait_params(
        "browser_wait_for_request",
        params.cdp_target_id.as_deref(),
        params.url.as_deref(),
        params.match_kind,
        params.method.as_deref(),
        None,
        params.resource_type.as_deref(),
        params.timeout_ms,
        params.polling_interval_ms,
    )
}

fn validate_browser_wait_for_response_params(
    params: &BrowserWaitForNetworkResponseParams,
) -> Result<NormalizedBrowserNetworkWaitParams, ErrorData> {
    validate_browser_network_wait_params(
        "browser_wait_for_response",
        params.cdp_target_id.as_deref(),
        params.url.as_deref(),
        params.match_kind,
        params.method.as_deref(),
        params.status,
        params.resource_type.as_deref(),
        params.timeout_ms,
        params.polling_interval_ms,
    )
}

fn validate_browser_network_wait_params(
    tool: &str,
    cdp_target_id: Option<&str>,
    url: Option<&str>,
    match_kind: Option<BrowserWaitForUrlMatchKind>,
    method: Option<&str>,
    status: Option<i64>,
    resource_type: Option<&str>,
    timeout_ms: Option<u64>,
    polling_interval_ms: Option<u64>,
) -> Result<NormalizedBrowserNetworkWaitParams, ErrorData> {
    if let Some(target_id) = cdp_target_id {
        validate_cdp_target_id(target_id)?;
    }
    let (url, match_kind, url_regex) = validate_browser_network_wait_url(tool, url, match_kind)?;
    let method = validate_browser_network_wait_token(tool, "method", method)?
        .map(|method| method.to_ascii_uppercase());
    let resource_type = validate_browser_network_wait_token(tool, "resource_type", resource_type)?;
    if let Some(status) = status
        && !(0..=999).contains(&status)
    {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{tool} status must be 0..=999"),
        ));
    }
    let timeout_ms = validate_browser_wait_timeout(tool, timeout_ms)?;
    let polling_interval_ms = validate_browser_wait_polling_interval(tool, polling_interval_ms)?;
    Ok(NormalizedBrowserNetworkWaitParams {
        url,
        match_kind,
        url_regex,
        method,
        status,
        resource_type,
        timeout_ms,
        polling_interval_ms,
    })
}

fn validate_browser_network_wait_url(
    tool: &str,
    url: Option<&str>,
    match_kind: Option<BrowserWaitForUrlMatchKind>,
) -> Result<
    (
        Option<String>,
        BrowserWaitForUrlMatchKind,
        Option<regex::Regex>,
    ),
    ErrorData,
> {
    let match_kind = match_kind.unwrap_or_default();
    let Some(url) = url else {
        if match_kind != BrowserWaitForUrlMatchKind::Exact {
            return Err(mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                format!("{tool} match_kind requires url"),
            ));
        }
        return Ok((None, match_kind, None));
    };
    if url.trim().is_empty() {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{tool} url must not be empty"),
        ));
    }
    if url.chars().count() > BROWSER_WAIT_MAX_URL_PATTERN_CHARS {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "{tool} url must be at most {BROWSER_WAIT_MAX_URL_PATTERN_CHARS} Unicode scalar values"
            ),
        ));
    }
    if url.contains('\0') {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{tool} url must not contain NUL"),
        ));
    }
    if url.trim() != url {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{tool} url must not contain leading or trailing whitespace"),
        ));
    }
    let url_regex = match match_kind {
        BrowserWaitForUrlMatchKind::Exact => None,
        BrowserWaitForUrlMatchKind::Glob => Some(compile_browser_wait_url_regex(
            tool,
            match_kind,
            &browser_wait_for_url_glob_regex(url),
        )?),
        BrowserWaitForUrlMatchKind::Regex => {
            Some(compile_browser_wait_url_regex(tool, match_kind, url)?)
        }
    };
    Ok((Some(url.to_owned()), match_kind, url_regex))
}

fn validate_browser_network_wait_token(
    tool: &str,
    field: &str,
    value: Option<&str>,
) -> Result<Option<String>, ErrorData> {
    let Some(value) = value else {
        return Ok(None);
    };
    if value.trim().is_empty() {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{tool} {field} must not be empty"),
        ));
    }
    if value.trim() != value {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{tool} {field} must not contain leading or trailing whitespace"),
        ));
    }
    if value.contains('\0') || value.chars().any(char::is_control) {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{tool} {field} must not contain control characters"),
        ));
    }
    if value.chars().count() > BROWSER_WAIT_MAX_NETWORK_TOKEN_CHARS {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "{tool} {field} must be at most {BROWSER_WAIT_MAX_NETWORK_TOKEN_CHARS} Unicode scalar values"
            ),
        ));
    }
    Ok(Some(value.to_owned()))
}

fn validate_browser_wait_for_selector_params(
    params: &BrowserWaitForSelectorParams,
) -> Result<NormalizedBrowserWaitForSelectorParams, ErrorData> {
    let locate = browser_wait_for_selector_locate_params(params);
    validate_browser_locate_like_params("browser_wait_for_selector", &locate)?;
    let timeout_ms = validate_browser_wait_timeout("browser_wait_for_selector", params.timeout_ms)?;
    let polling_interval_ms = validate_browser_wait_polling_interval(
        "browser_wait_for_selector",
        params.polling_interval_ms,
    )?;
    let limit = locate
        .limit
        .unwrap_or(DEFAULT_BROWSER_LOCATE_LIMIT)
        .clamp(1, MAX_BROWSER_LOCATE_LIMIT);
    Ok(NormalizedBrowserWaitForSelectorParams {
        locate,
        state: params.state.unwrap_or_default(),
        timeout_ms,
        polling_interval_ms,
        limit,
    })
}

fn validate_browser_locate_like_params(
    tool: &str,
    params: &BrowserLocateParams,
) -> Result<(), ErrorData> {
    if params.query.trim().is_empty() {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{tool} requires a non-empty query"),
        ));
    }
    if params.query.len() > BROWSER_LOCATE_MAX_SELECTOR_BYTES {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "{tool} query is {} bytes; the maximum is {BROWSER_LOCATE_MAX_SELECTOR_BYTES}",
                params.query.len()
            ),
        ));
    }
    if params.exact == Some(true) && params.regex == Some(true) {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{tool} exact and regex are mutually exclusive"),
        ));
    }
    if params.name_exact == Some(true) && params.name_regex == Some(true) {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{tool} name_exact and name_regex are mutually exclusive"),
        ));
    }
    if params.engine == BrowserLocateEngine::Layout {
        if params.relation.is_none() {
            return Err(mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                format!(
                    "{tool} layout engine requires `relation` (near|right-of|left-of|above|below)"
                ),
            ));
        }
        if params
            .anchor
            .as_deref()
            .is_none_or(|anchor| anchor.trim().is_empty())
        {
            return Err(mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                format!("{tool} layout engine requires a non-empty `anchor` CSS selector"),
            ));
        }
    }
    if let Some(target_id) = params.cdp_target_id.as_deref() {
        validate_cdp_target_id(target_id)?;
    }
    validate_browser_frame_locator(tool, params.frame.as_ref())?;
    Ok(())
}

fn validate_browser_frame_locator(
    tool: &str,
    frame: Option<&BrowserFrameLocator>,
) -> Result<(), ErrorData> {
    let Some(frame) = frame else {
        return Ok(());
    };
    let mut selectors = 0_u8;
    for (field, value) in [
        ("frame_id", frame.frame_id.as_deref()),
        ("frame_element_id", frame.frame_element_id.as_deref()),
        ("name", frame.name.as_deref()),
        ("url", frame.url.as_deref()),
    ] {
        if let Some(value) = value {
            if value.trim().is_empty() {
                return Err(mcp_error(
                    error_codes::TOOL_PARAMS_INVALID,
                    format!("{tool} frame.{field} must not be empty when supplied"),
                ));
            }
            selectors = selectors.saturating_add(1);
        }
    }
    if frame.index.is_some() {
        selectors = selectors.saturating_add(1);
    }
    if selectors != 1 {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "{tool} frame locator requires exactly one of frame_id, frame_element_id, name, url, or index"
            ),
        ));
    }
    Ok(())
}

fn validate_browser_wait_for_function_params(
    params: &BrowserWaitForFunctionParams,
) -> Result<NormalizedBrowserWaitForFunctionParams, ErrorData> {
    if params.expression.trim().is_empty() {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "browser_wait_for_function requires a non-empty expression",
        ));
    }
    if params.expression.len() > BROWSER_EVALUATE_MAX_EXPRESSION_BYTES {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "browser_wait_for_function expression is {} bytes; the maximum is {BROWSER_EVALUATE_MAX_EXPRESSION_BYTES} bytes",
                params.expression.len()
            ),
        ));
    }
    if let Some(target_id) = params.cdp_target_id.as_deref() {
        validate_cdp_target_id(target_id)?;
    }
    let timeout_ms = validate_browser_wait_timeout("browser_wait_for_function", params.timeout_ms)?;
    let polling_interval_ms = validate_browser_wait_polling_interval(
        "browser_wait_for_function",
        params.polling_interval_ms,
    )?;
    let args = params.args.clone().unwrap_or_default();
    if args.len() > BROWSER_EVALUATE_MAX_ARGS {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "browser_wait_for_function accepts at most {BROWSER_EVALUATE_MAX_ARGS} args; got {}",
                args.len()
            ),
        ));
    }
    Ok(NormalizedBrowserWaitForFunctionParams {
        expression: params.expression.clone(),
        args,
        timeout_ms,
        polling_interval_ms,
    })
}

fn validate_browser_wait_for_params(
    params: &BrowserWaitForParams,
) -> Result<NormalizedBrowserWaitForParams, ErrorData> {
    if let Some(target_id) = params.cdp_target_id.as_deref() {
        validate_cdp_target_id(target_id)?;
    }
    let timeout_ms = validate_browser_wait_timeout("browser_wait_for", params.timeout_ms)?;
    let polling_interval_ms =
        validate_browser_wait_polling_interval("browser_wait_for", params.polling_interval_ms)?;
    let text = params.text.as_ref().map(|text| text.to_owned());
    if let Some(text) = text.as_deref() {
        if text.trim().is_empty() {
            return Err(mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                "browser_wait_for text must not be empty when supplied",
            ));
        }
        if text.len() > BROWSER_WAIT_MAX_TEXT_BYTES {
            return Err(mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                format!(
                    "browser_wait_for text is {} bytes; the maximum is {BROWSER_WAIT_MAX_TEXT_BYTES} bytes",
                    text.len()
                ),
            ));
        }
        if text.contains('\0') {
            return Err(mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                "browser_wait_for text must not contain NUL",
            ));
        }
    }
    let state = match (params.state, text.as_ref()) {
        (Some(BrowserWaitForState::TextAppears), Some(_)) => BrowserWaitForState::TextAppears,
        (Some(BrowserWaitForState::TextGone), Some(_)) => BrowserWaitForState::TextGone,
        (Some(BrowserWaitForState::Timeout) | None, None) => BrowserWaitForState::Timeout,
        (None, Some(_)) => BrowserWaitForState::TextAppears,
        (Some(BrowserWaitForState::Timeout), Some(_)) => {
            return Err(mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                "browser_wait_for state=timeout does not accept text",
            ));
        }
        (Some(BrowserWaitForState::TextAppears | BrowserWaitForState::TextGone), None) => {
            return Err(mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                "browser_wait_for state=text_appears/text_gone requires text",
            ));
        }
    };
    Ok(NormalizedBrowserWaitForParams {
        state,
        text,
        timeout_ms,
        polling_interval_ms,
    })
}

fn validate_browser_wait_timeout(tool: &str, value: Option<u64>) -> Result<u64, ErrorData> {
    let timeout_ms = value.unwrap_or(DEFAULT_BROWSER_WAIT_TIMEOUT_MS);
    if timeout_ms == 0 || timeout_ms > MAX_BROWSER_WAIT_TIMEOUT_MS {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{tool} timeout_ms must be 1..={MAX_BROWSER_WAIT_TIMEOUT_MS}"),
        ));
    }
    Ok(timeout_ms)
}

fn validate_browser_wait_polling_interval(
    tool: &str,
    value: Option<u64>,
) -> Result<u64, ErrorData> {
    let polling_interval_ms = value.unwrap_or(DEFAULT_BROWSER_WAIT_POLLING_INTERVAL_MS);
    if !(MIN_BROWSER_WAIT_POLLING_INTERVAL_MS..=MAX_BROWSER_WAIT_POLLING_INTERVAL_MS)
        .contains(&polling_interval_ms)
    {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "{tool} polling_interval_ms must be {MIN_BROWSER_WAIT_POLLING_INTERVAL_MS}..={MAX_BROWSER_WAIT_POLLING_INTERVAL_MS}"
            ),
        ));
    }
    Ok(polling_interval_ms)
}

fn validate_browser_wait_for_url_pattern(
    match_kind: BrowserWaitForUrlMatchKind,
    pattern: &str,
) -> Result<(), ErrorData> {
    let regex_pattern = match match_kind {
        BrowserWaitForUrlMatchKind::Exact => return Ok(()),
        BrowserWaitForUrlMatchKind::Glob => browser_wait_for_url_glob_regex(pattern),
        BrowserWaitForUrlMatchKind::Regex => pattern.to_owned(),
    };
    compile_browser_wait_url_regex("browser_wait_for_url", match_kind, &regex_pattern)?;
    Ok(())
}

fn compile_browser_wait_url_regex(
    tool: &str,
    match_kind: BrowserWaitForUrlMatchKind,
    pattern: &str,
) -> Result<regex::Regex, ErrorData> {
    regex::Regex::new(pattern).map_err(|error| {
        mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{tool} {match_kind:?} pattern is invalid: {error}"),
        )
    })
}

fn browser_wait_for_url_glob_regex(glob: &str) -> String {
    let mut regex = String::from("^");
    for ch in glob.chars() {
        match ch {
            '*' => regex.push_str(".*"),
            '?' => regex.push('.'),
            _ => regex.push_str(&regex::escape(&ch.to_string())),
        }
    }
    regex.push('$');
    regex
}

fn browser_network_entry_matches(
    entry: &synapse_a11y::CdpNetworkEntry,
    wait: &NormalizedBrowserNetworkWaitParams,
    require_response: bool,
) -> bool {
    if require_response && !(entry.response_received && entry.response.is_some()) {
        return false;
    }
    if let Some(method) = wait.method.as_deref()
        && !entry
            .method
            .as_deref()
            .is_some_and(|entry_method| entry_method.eq_ignore_ascii_case(method))
    {
        return false;
    }
    if let Some(status) = wait.status
        && entry.response.as_ref().map(|response| response.status) != Some(status)
    {
        return false;
    }
    if let Some(resource_type) = wait.resource_type.as_deref()
        && !entry
            .resource_type
            .as_deref()
            .is_some_and(|entry_type| entry_type.eq_ignore_ascii_case(resource_type))
    {
        return false;
    }
    if let Some(url) = wait.url.as_deref() {
        let candidate = if require_response {
            entry
                .response
                .as_ref()
                .map(|response| response.url.as_str())
                .or(entry.url.as_deref())
        } else {
            entry.url.as_deref()
        };
        let Some(candidate) = candidate else {
            return false;
        };
        return match wait.match_kind {
            BrowserWaitForUrlMatchKind::Exact => candidate == url,
            BrowserWaitForUrlMatchKind::Glob | BrowserWaitForUrlMatchKind::Regex => wait
                .url_regex
                .as_ref()
                .is_some_and(|regex| regex.is_match(candidate)),
        };
    }
    true
}

fn browser_network_entry_to_wire(entry: &synapse_a11y::CdpNetworkEntry) -> BrowserNetworkWaitEntry {
    let response = entry.response.as_ref();
    BrowserNetworkWaitEntry {
        seq: entry.seq,
        request_id: entry.request_id.clone(),
        url: redact_url_opt_for_public_readback(entry.url.clone()),
        method: entry.method.clone(),
        resource_type: entry.resource_type.clone(),
        request_headers: entry.request_headers.clone(),
        response_received: entry.response_received,
        response_url: response.map(|response| redact_url_for_public_readback(&response.url)),
        status: response.map(|response| response.status),
        status_text: response.map(|response| response.status_text.clone()),
        response_headers: response.map(|response| response.headers.clone()),
        response_timing: response.and_then(|response| response.timing.clone()),
        protocol: response.and_then(|response| response.protocol.clone()),
        remote_ip_address: response.and_then(|response| response.remote_ip_address.clone()),
        remote_port: response.and_then(|response| response.remote_port),
        encoded_data_length: entry
            .encoded_data_length
            .or_else(|| response.map(|response| response.encoded_data_length)),
        loading_finished: entry.loading_finished,
        loading_failed: entry.loading_failed,
        failure_error_text: entry.failure_error_text.clone(),
    }
}

#[cfg(windows)]
fn chrome_bridge_network_entry_to_wire(
    entry: crate::chrome_debugger_bridge::ChromeDebuggerNetworkWaitEntry,
) -> BrowserNetworkWaitEntry {
    BrowserNetworkWaitEntry {
        seq: entry.seq,
        request_id: entry.request_id,
        url: redact_url_opt_for_public_readback(entry.url),
        method: entry.method,
        resource_type: entry.resource_type,
        request_headers: entry.request_headers,
        response_received: entry.response_received,
        response_url: redact_url_opt_for_public_readback(entry.response_url),
        status: entry.status,
        status_text: entry.status_text,
        response_headers: entry.response_headers,
        response_timing: entry.response_timing,
        protocol: entry.protocol,
        remote_ip_address: entry.remote_ip_address,
        remote_port: entry.remote_port,
        encoded_data_length: entry.encoded_data_length,
        loading_finished: entry.loading_finished,
        loading_failed: entry.loading_failed,
        failure_error_text: entry.failure_error_text,
    }
}

fn browser_wait_for_selector_locate_params(
    params: &BrowserWaitForSelectorParams,
) -> BrowserLocateParams {
    BrowserLocateParams {
        query: params.query.clone(),
        engine: params.engine,
        exact: params.exact,
        regex: params.regex,
        name: params.name.clone(),
        name_exact: params.name_exact,
        name_regex: params.name_regex,
        testid_attribute: params.testid_attribute.clone(),
        checked: params.checked,
        pressed: params.pressed,
        expanded: params.expanded,
        selected: params.selected,
        disabled: params.disabled,
        level: params.level,
        include_hidden: params.include_hidden,
        relation: params.relation,
        anchor: params.anchor.clone(),
        max_distance: params.max_distance,
        has_text: params.has_text.clone(),
        nth: params.nth,
        strict: params.strict,
        root_element_id: params.root_element_id.clone(),
        frame: params.frame.clone(),
        cdp_target_id: params.cdp_target_id.clone(),
        window_hwnd: params.window_hwnd,
        limit: params.limit,
    }
}

#[cfg(windows)]
fn build_browser_wait_for_function_expression(
    wait: &NormalizedBrowserWaitForFunctionParams,
) -> Result<String, ErrorData> {
    let args_json = serde_json::to_string(&wait.args).map_err(|error| {
        mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("browser_wait_for_function could not serialize args: {error}"),
        )
    })?;
    let predicate = &wait.expression;
    let timeout_ms = wait.timeout_ms;
    let polling_interval_ms = wait.polling_interval_ms;
    let expression = format!(
        r#"(() => new Promise((resolve, reject) => {{
            const args = {args_json};
            const timeoutMs = {timeout_ms};
            const pollingIntervalMs = {polling_interval_ms};
            const started = Date.now();
            let pollCount = 0;
            const serializeValue = (value) => {{
                const valueType = typeof value;
                if (value === undefined) {{
                    return {{ value: null, value_type: "undefined", value_description: "undefined", unserializable_value: null }};
                }}
                if (valueType === "bigint") {{
                    return {{ value: String(value), value_type: valueType, value_description: String(value) + "n", unserializable_value: String(value) + "n" }};
                }}
                if (valueType === "number" && !Number.isFinite(value)) {{
                    return {{ value: null, value_type: valueType, value_description: String(value), unserializable_value: String(value) }};
                }}
                if (valueType === "function" || valueType === "symbol") {{
                    return {{ value: null, value_type: valueType, value_description: String(value), unserializable_value: null }};
                }}
                try {{
                    return {{ value: JSON.parse(JSON.stringify(value)), value_type: valueType, value_description: null, unserializable_value: null }};
                }} catch (error) {{
                    return {{ value: null, value_type: valueType, value_description: String(value), unserializable_value: null }};
                }}
            }};
            const finish = (conditionMet, timedOut, value) => {{
                const serialized = serializeValue(value);
                resolve({{
                    condition_met: conditionMet,
                    timed_out: timedOut,
                    elapsed_ms: Math.max(0, Date.now() - started),
                    poll_count: pollCount,
                    value: serialized.value,
                    value_type: serialized.value_type,
                    value_description: serialized.value_description,
                    unserializable_value: serialized.unserializable_value
                }});
            }};
            const evaluatePredicate = async () => {{
                const candidate = ({predicate});
                const value = (typeof candidate === "function") ? candidate(...args) : candidate;
                return await Promise.resolve(value);
            }};
            const check = async () => {{
                pollCount += 1;
                let value;
                try {{
                    value = await evaluatePredicate();
                }} catch (error) {{
                    reject(error);
                    return;
                }}
                if (value) {{
                    finish(true, false, value);
                    return;
                }}
                if (Date.now() - started >= timeoutMs) {{
                    finish(false, true, value);
                    return;
                }}
                window.setTimeout(check, pollingIntervalMs);
            }};
            check();
        }}))()"#
    );
    if expression.len() > BROWSER_EVALUATE_MAX_EXPRESSION_BYTES {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "browser_wait_for_function generated Runtime.evaluate expression is {} bytes after JSON escaping; the maximum is {BROWSER_EVALUATE_MAX_EXPRESSION_BYTES} bytes",
                expression.len()
            ),
        ));
    }
    Ok(expression)
}

#[cfg(windows)]
fn build_browser_wait_for_expression(
    wait: &NormalizedBrowserWaitForParams,
) -> Result<String, ErrorData> {
    let state = match wait.state {
        BrowserWaitForState::TextAppears => "text_appears",
        BrowserWaitForState::TextGone => "text_gone",
        BrowserWaitForState::Timeout => "timeout",
    };
    let state_json = browser_tag_json_string("browser_wait_for", "state", state)?;
    let text_json = browser_tag_json_string(
        "browser_wait_for",
        "text",
        wait.text.as_deref().unwrap_or_default(),
    )?;
    let timeout_ms = wait.timeout_ms;
    let polling_interval_ms = wait.polling_interval_ms;
    let expression = format!(
        r#"(() => new Promise((resolve) => {{
            const state = {state_json};
            const expectedText = {text_json};
            const timeoutMs = {timeout_ms};
            const pollingIntervalMs = {polling_interval_ms};
            const started = Date.now();
            let pollCount = 0;
            let lastText = "";
            const readText = () => {{
                const root = document.body || document.documentElement;
                if (!root) {{
                    return "";
                }}
                const inner = typeof root.innerText === "string" ? root.innerText : "";
                const textContent = typeof root.textContent === "string" ? root.textContent : "";
                return inner || textContent;
            }};
            const finish = (conditionMet, timedOut) => resolve({{
                condition_met: conditionMet,
                timed_out: timedOut,
                elapsed_ms: Math.max(0, Date.now() - started),
                poll_count: pollCount,
                observed_text_len: lastText.length
            }});
            if (state === "timeout") {{
                window.setTimeout(() => {{
                    lastText = readText();
                    finish(true, false);
                }}, timeoutMs);
                return;
            }}
            const check = () => {{
                pollCount += 1;
                lastText = readText();
                const contains = lastText.includes(expectedText);
                const conditionMet = state === "text_gone" ? !contains : contains;
                if (conditionMet) {{
                    finish(true, false);
                    return;
                }}
                if (Date.now() - started >= timeoutMs) {{
                    finish(false, true);
                    return;
                }}
                window.setTimeout(check, pollingIntervalMs);
            }};
            check();
        }}))()"#
    );
    if expression.len() > BROWSER_EVALUATE_MAX_EXPRESSION_BYTES {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "browser_wait_for generated Runtime.evaluate expression is {} bytes after JSON escaping; the maximum is {BROWSER_EVALUATE_MAX_EXPRESSION_BYTES} bytes",
                expression.len()
            ),
        ));
    }
    Ok(expression)
}

fn validate_browser_add_script_tag_params(
    params: &BrowserAddScriptTagParams,
) -> Result<(), ErrorData> {
    validate_browser_tag_common_params(
        "browser_add_script_tag",
        params.cdp_target_id.as_deref(),
        params.url.as_deref(),
        params.content.as_deref(),
        params.path.as_deref(),
    )?;
    if let Some(script_type) = params.script_type.as_deref() {
        validate_browser_tag_script_type(script_type)?;
    }
    Ok(())
}

fn validate_browser_add_style_tag_params(
    params: &BrowserAddStyleTagParams,
) -> Result<(), ErrorData> {
    validate_browser_tag_common_params(
        "browser_add_style_tag",
        params.cdp_target_id.as_deref(),
        params.url.as_deref(),
        params.content.as_deref(),
        params.path.as_deref(),
    )
}

fn validate_browser_tag_common_params(
    tool: &str,
    cdp_target_id: Option<&str>,
    url: Option<&str>,
    content: Option<&str>,
    path: Option<&str>,
) -> Result<(), ErrorData> {
    if let Some(target_id) = cdp_target_id {
        validate_cdp_target_id(target_id)?;
    }
    let source_count = [url.is_some(), content.is_some(), path.is_some()]
        .into_iter()
        .filter(|supplied| *supplied)
        .count();
    if source_count != 1 {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{tool} requires exactly one of url, content, or path"),
        ));
    }
    if let Some(url) = url {
        validate_browser_tag_url(tool, url)?;
    }
    if let Some(content) = content {
        validate_browser_tag_content(tool, "content", content)?;
    }
    if let Some(path) = path {
        validate_browser_tag_path_param(tool, path)?;
    }
    Ok(())
}

fn validate_browser_tag_url(tool: &str, url: &str) -> Result<(), ErrorData> {
    if url.trim().is_empty() {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{tool} url must not be empty"),
        ));
    }
    if url.chars().count() > BROWSER_TAG_MAX_URL_CHARS {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{tool} url must be at most {BROWSER_TAG_MAX_URL_CHARS} Unicode scalar values"),
        ));
    }
    if url.contains('\0') {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{tool} url must not contain NUL"),
        ));
    }
    if url.trim() != url {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{tool} url must not contain leading or trailing whitespace"),
        ));
    }
    Ok(())
}

fn validate_browser_tag_content(tool: &str, field: &str, content: &str) -> Result<(), ErrorData> {
    if content.trim().is_empty() {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{tool} {field} must not be empty"),
        ));
    }
    if content.len() > BROWSER_TAG_MAX_CONTENT_BYTES {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "{tool} {field} is {} bytes; the maximum is {BROWSER_TAG_MAX_CONTENT_BYTES} bytes",
                content.len()
            ),
        ));
    }
    Ok(())
}

fn validate_browser_tag_path_param(tool: &str, path: &str) -> Result<(), ErrorData> {
    if path.trim().is_empty() {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{tool} path must not be empty"),
        ));
    }
    if path.chars().count() > BROWSER_TAG_MAX_PATH_CHARS {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "{tool} path must be at most {BROWSER_TAG_MAX_PATH_CHARS} Unicode scalar values"
            ),
        ));
    }
    if path.contains('\0') {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{tool} path must not contain NUL"),
        ));
    }
    if path.trim() != path {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{tool} path must not contain leading or trailing whitespace"),
        ));
    }
    Ok(())
}

fn validate_browser_tag_script_type(script_type: &str) -> Result<(), ErrorData> {
    if script_type.trim().is_empty() {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "browser_add_script_tag script_type must not be empty when supplied",
        ));
    }
    if script_type.chars().count() > BROWSER_TAG_MAX_SCRIPT_TYPE_CHARS {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "browser_add_script_tag script_type must be at most {BROWSER_TAG_MAX_SCRIPT_TYPE_CHARS} Unicode scalar values"
            ),
        ));
    }
    if script_type.contains('\0') {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "browser_add_script_tag script_type must not contain NUL",
        ));
    }
    if script_type.trim() != script_type {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "browser_add_script_tag script_type must not contain leading or trailing whitespace",
        ));
    }
    Ok(())
}

fn resolve_browser_tag_source(
    tool: &str,
    url: Option<&str>,
    content: Option<&str>,
    path: Option<&str>,
) -> Result<ResolvedBrowserTagSource, ErrorData> {
    if let Some(url) = url {
        return Ok(ResolvedBrowserTagSource {
            kind: BrowserTagSourceKind::Url,
            requested_url: Some(url.to_owned()),
            path: None,
            content: String::new(),
            content_len: 0,
        });
    }
    if let Some(content) = content {
        return Ok(ResolvedBrowserTagSource {
            kind: BrowserTagSourceKind::Content,
            requested_url: None,
            path: None,
            content: content.to_owned(),
            content_len: content.len(),
        });
    }
    let Some(path) = path else {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{tool} requires exactly one of url, content, or path"),
        ));
    };
    let requested = PathBuf::from(path);
    let canonical = std::fs::canonicalize(&requested).map_err(|error| {
        mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{tool} path must be an existing file, got {path:?}: {error}"),
        )
    })?;
    let metadata = std::fs::metadata(&canonical).map_err(|error| {
        mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "{tool} failed to read metadata for {}: {error}",
                canonical.display()
            ),
        )
    })?;
    if !metadata.is_file() {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{tool} path must be a file, got {}", canonical.display()),
        ));
    }
    if metadata.len() > BROWSER_TAG_MAX_CONTENT_BYTES as u64 {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "{tool} path content is {} bytes; the maximum is {BROWSER_TAG_MAX_CONTENT_BYTES} bytes",
                metadata.len()
            ),
        ));
    }
    let bytes = std::fs::read(&canonical).map_err(|error| {
        mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{tool} failed to read {}: {error}", canonical.display()),
        )
    })?;
    if bytes.len() > BROWSER_TAG_MAX_CONTENT_BYTES {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "{tool} path content is {} bytes; the maximum is {BROWSER_TAG_MAX_CONTENT_BYTES} bytes",
                bytes.len()
            ),
        ));
    }
    let content = String::from_utf8(bytes).map_err(|error| {
        mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{tool} path must contain UTF-8 text: {error}"),
        )
    })?;
    validate_browser_tag_content(tool, "path content", &content)?;
    let content_len = content.len();
    Ok(ResolvedBrowserTagSource {
        kind: BrowserTagSourceKind::Path,
        requested_url: None,
        path: Some(canonical.to_string_lossy().into_owned()),
        content,
        content_len,
    })
}

#[cfg(windows)]
fn browser_tag_marker(tool: &str, cdp_target_id: &str) -> String {
    let timestamp_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let digest = sha256_hex(format!("{tool}:{cdp_target_id}:{timestamp_ms}").as_bytes());
    format!("synapse-{tool}-{timestamp_ms}-{}", &digest[..12])
}

#[cfg(windows)]
fn browser_tag_json_string(tool: &str, field: &str, value: &str) -> Result<String, ErrorData> {
    serde_json::to_string(value).map_err(|error| {
        mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("{tool} failed to encode {field} for injection: {error}"),
        )
    })
}

#[cfg(windows)]
fn build_browser_add_tag_expression(
    tool: &str,
    tag_kind: BrowserTagKind,
    source: &ResolvedBrowserTagSource,
    script_type: Option<&str>,
    marker: &str,
) -> Result<String, ErrorData> {
    let marker_json = browser_tag_json_string(tool, "element marker", marker)?;
    let expression = match (tag_kind, source.kind) {
        (BrowserTagKind::Script, BrowserTagSourceKind::Url) => {
            let url_json = browser_tag_json_string(
                tool,
                "url",
                source.requested_url.as_deref().unwrap_or_default(),
            )?;
            let script_type_json =
                browser_tag_json_string(tool, "script_type", script_type.unwrap_or_default())?;
            format!(
                r#"(() => new Promise((resolve, reject) => {{
                    const marker = {marker_json};
                    const requestedUrl = {url_json};
                    const scriptType = {script_type_json};
                    const parent = document.head || document.documentElement || document.body;
                    if (!parent) {{
                        reject(new Error("{tool} failed: document has no append target"));
                        return;
                    }}
                    const el = document.createElement("script");
                    el.setAttribute("data-synapse-tag-id", marker);
                    if (scriptType) {{
                        el.type = scriptType;
                    }}
                    el.onload = () => resolve({{
                        tag_name: el.tagName.toLowerCase(),
                        source_kind: "url",
                        requested_url: requestedUrl,
                        resolved_url: el.src || "",
                        content_len: 0,
                        element_marker: marker
                    }});
                    el.onerror = () => reject(new Error("{tool} url load failed: " + requestedUrl));
                    el.src = requestedUrl;
                    parent.appendChild(el);
                }}))()"#
            )
        }
        (BrowserTagKind::Script, BrowserTagSourceKind::Content | BrowserTagSourceKind::Path) => {
            let content_json = browser_tag_json_string(tool, "content", &source.content)?;
            let source_kind_json =
                browser_tag_json_string(tool, "source_kind", source.kind.as_str())?;
            let script_type_json =
                browser_tag_json_string(tool, "script_type", script_type.unwrap_or_default())?;
            let content_len = source.content_len;
            format!(
                r#"(() => {{
                    const marker = {marker_json};
                    const source = {content_json};
                    const sourceKind = {source_kind_json};
                    const scriptType = {script_type_json};
                    const parent = document.head || document.documentElement || document.body;
                    if (!parent) {{
                        throw new Error("{tool} failed: document has no append target");
                    }}
                    const el = document.createElement("script");
                    el.setAttribute("data-synapse-tag-id", marker);
                    if (scriptType) {{
                        el.type = scriptType;
                    }}
                    el.textContent = source;
                    parent.appendChild(el);
                    return {{
                        tag_name: el.tagName.toLowerCase(),
                        source_kind: sourceKind,
                        requested_url: null,
                        resolved_url: null,
                        content_len: {content_len},
                        element_marker: marker
                    }};
                }})()"#
            )
        }
        (BrowserTagKind::Style, BrowserTagSourceKind::Url) => {
            let url_json = browser_tag_json_string(
                tool,
                "url",
                source.requested_url.as_deref().unwrap_or_default(),
            )?;
            format!(
                r#"(() => new Promise((resolve, reject) => {{
                    const marker = {marker_json};
                    const requestedUrl = {url_json};
                    const parent = document.head || document.documentElement || document.body;
                    if (!parent) {{
                        reject(new Error("{tool} failed: document has no append target"));
                        return;
                    }}
                    const el = document.createElement("link");
                    el.setAttribute("data-synapse-tag-id", marker);
                    el.rel = "stylesheet";
                    el.onload = () => resolve({{
                        tag_name: el.tagName.toLowerCase(),
                        source_kind: "url",
                        requested_url: requestedUrl,
                        resolved_url: el.href || "",
                        content_len: 0,
                        element_marker: marker
                    }});
                    el.onerror = () => reject(new Error("{tool} url load failed: " + requestedUrl));
                    el.href = requestedUrl;
                    parent.appendChild(el);
                }}))()"#
            )
        }
        (BrowserTagKind::Style, BrowserTagSourceKind::Content | BrowserTagSourceKind::Path) => {
            let content_json = browser_tag_json_string(tool, "content", &source.content)?;
            let source_kind_json =
                browser_tag_json_string(tool, "source_kind", source.kind.as_str())?;
            let content_len = source.content_len;
            format!(
                r#"(() => {{
                    const marker = {marker_json};
                    const source = {content_json};
                    const sourceKind = {source_kind_json};
                    const parent = document.head || document.documentElement || document.body;
                    if (!parent) {{
                        throw new Error("{tool} failed: document has no append target");
                    }}
                    const el = document.createElement("style");
                    el.setAttribute("data-synapse-tag-id", marker);
                    el.textContent = source;
                    parent.appendChild(el);
                    return {{
                        tag_name: el.tagName.toLowerCase(),
                        source_kind: sourceKind,
                        requested_url: null,
                        resolved_url: null,
                        content_len: {content_len},
                        element_marker: marker
                    }};
                }})()"#
            )
        }
    };
    if expression.len() > BROWSER_EVALUATE_MAX_EXPRESSION_BYTES {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "{tool} generated Runtime.evaluate expression is {} bytes after JSON escaping; the maximum is {BROWSER_EVALUATE_MAX_EXPRESSION_BYTES} bytes",
                expression.len()
            ),
        ));
    }
    Ok(expression)
}

fn validate_browser_add_init_script_params(
    params: &BrowserAddInitScriptParams,
) -> Result<(), ErrorData> {
    if let Some(target_id) = params.cdp_target_id.as_deref() {
        validate_cdp_target_id(target_id)?;
    }
    if let Some(world_name) = params.world_name.as_deref() {
        validate_browser_init_script_world_name(world_name)?;
    }
    match params.operation {
        BrowserInitScriptOperation::Add => {
            let source = params.source.as_deref().ok_or_else(|| {
                mcp_error(
                    error_codes::TOOL_PARAMS_INVALID,
                    "browser_add_init_script operation=add requires source",
                )
            })?;
            if source.trim().is_empty() {
                return Err(mcp_error(
                    error_codes::TOOL_PARAMS_INVALID,
                    "browser_add_init_script source must not be empty",
                ));
            }
            if source.len() > BROWSER_INIT_SCRIPT_MAX_SOURCE_BYTES {
                return Err(mcp_error(
                    error_codes::TOOL_PARAMS_INVALID,
                    format!(
                        "browser_add_init_script source is {} bytes; the maximum is {BROWSER_INIT_SCRIPT_MAX_SOURCE_BYTES} bytes",
                        source.len()
                    ),
                ));
            }
            if let Some(identifier) = params.identifier.as_deref()
                && !identifier.trim().is_empty()
            {
                return Err(mcp_error(
                    error_codes::TOOL_PARAMS_INVALID,
                    "browser_add_init_script operation=add returns identifier; do not supply one",
                ));
            }
        }
        BrowserInitScriptOperation::Remove => {
            let identifier = params.identifier.as_deref().ok_or_else(|| {
                mcp_error(
                    error_codes::TOOL_PARAMS_INVALID,
                    "browser_add_init_script operation=remove requires identifier",
                )
            })?;
            validate_browser_init_script_identifier(identifier)?;
            if params.source.is_some()
                || params.world_name.is_some()
                || params.include_command_line_api.is_some()
                || params.run_immediately.is_some()
            {
                return Err(mcp_error(
                    error_codes::TOOL_PARAMS_INVALID,
                    "browser_add_init_script operation=remove only accepts cdp_target_id, window_hwnd, operation, and identifier",
                ));
            }
        }
    }
    Ok(())
}

fn validate_browser_expose_binding_params(
    params: &BrowserExposeBindingParams,
) -> Result<usize, ErrorData> {
    if let Some(target_id) = params.cdp_target_id.as_deref() {
        validate_cdp_target_id(target_id)?;
    }
    validate_browser_binding_name(&params.name)?;
    if let Some(execution_context_name) = params.execution_context_name.as_deref() {
        validate_browser_init_script_world_name(execution_context_name)?;
    }
    if params.operation != BrowserExposeBindingOperation::Add
        && params.execution_context_name.is_some()
    {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "browser_expose_binding execution_context_name is only valid for operation=add",
        ));
    }
    let max_calls = params
        .max_calls
        .unwrap_or(DEFAULT_BROWSER_BINDING_CALLS)
        .min(MAX_BROWSER_BINDING_CALLS);
    Ok(max_calls)
}

fn validate_browser_binding_name(name: &str) -> Result<(), ErrorData> {
    if name.trim() != name || name.is_empty() {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "browser_expose_binding name must be a non-empty JavaScript identifier without leading or trailing whitespace",
        ));
    }
    if name.chars().count() > BROWSER_BINDING_NAME_MAX_CHARS {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "browser_expose_binding name must be at most {BROWSER_BINDING_NAME_MAX_CHARS} Unicode scalar values"
            ),
        ));
    }
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "browser_expose_binding name must not be empty",
        ));
    };
    if !is_js_identifier_start(first) || !chars.all(is_js_identifier_continue) {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "browser_expose_binding name must be an ASCII JavaScript identifier, e.g. myBinding",
        ));
    }
    Ok(())
}

fn is_js_identifier_start(ch: char) -> bool {
    ch == '_' || ch == '$' || ch.is_ascii_alphabetic()
}

fn is_js_identifier_continue(ch: char) -> bool {
    is_js_identifier_start(ch) || ch.is_ascii_digit()
}

fn validate_browser_init_script_identifier(identifier: &str) -> Result<(), ErrorData> {
    if identifier.trim().is_empty() {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "browser_add_init_script identifier must not be empty",
        ));
    }
    if identifier.chars().count() > BROWSER_INIT_SCRIPT_MAX_IDENTIFIER_CHARS {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "browser_add_init_script identifier must be at most {BROWSER_INIT_SCRIPT_MAX_IDENTIFIER_CHARS} Unicode scalar values"
            ),
        ));
    }
    if identifier.contains('\0') {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "browser_add_init_script identifier must not contain NUL",
        ));
    }
    Ok(())
}

fn validate_browser_init_script_world_name(world_name: &str) -> Result<(), ErrorData> {
    if world_name.trim().is_empty() {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "browser_add_init_script world_name must not be empty when supplied",
        ));
    }
    if world_name.chars().count() > BROWSER_INIT_SCRIPT_MAX_WORLD_NAME_CHARS {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "browser_add_init_script world_name must be at most {BROWSER_INIT_SCRIPT_MAX_WORLD_NAME_CHARS} Unicode scalar values"
            ),
        ));
    }
    if world_name.contains('\0') {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "browser_add_init_script world_name must not contain NUL",
        ));
    }
    Ok(())
}

const BROWSER_EVALUATE_MAX_ARGS: usize = 64;

const DEFAULT_BROWSER_LOCATE_LIMIT: usize = 50;
const MAX_BROWSER_LOCATE_LIMIT: usize = 500;
const BROWSER_LOCATE_MAX_SELECTOR_BYTES: usize = 16 * 1024;

/// Maps the MCP `browser_locate` engine onto the a11y selector engine.
#[cfg(windows)]
fn browser_locate_engine_to_a11y(engine: BrowserLocateEngine) -> synapse_a11y::CdpLocateEngine {
    match engine {
        BrowserLocateEngine::Css => synapse_a11y::CdpLocateEngine::Css,
        BrowserLocateEngine::Xpath => synapse_a11y::CdpLocateEngine::Xpath,
        BrowserLocateEngine::Text => synapse_a11y::CdpLocateEngine::Text,
        BrowserLocateEngine::Role => synapse_a11y::CdpLocateEngine::Role,
        BrowserLocateEngine::Label => synapse_a11y::CdpLocateEngine::Label,
        BrowserLocateEngine::Placeholder => synapse_a11y::CdpLocateEngine::Placeholder,
        BrowserLocateEngine::AltText => synapse_a11y::CdpLocateEngine::AltText,
        BrowserLocateEngine::Title => synapse_a11y::CdpLocateEngine::Title,
        BrowserLocateEngine::TestId => synapse_a11y::CdpLocateEngine::TestId,
        BrowserLocateEngine::Layout => synapse_a11y::CdpLocateEngine::Layout,
    }
}

#[cfg(windows)]
fn browser_wait_for_load_state_to_a11y(
    state: BrowserWaitForLoadStateState,
) -> synapse_a11y::CdpLoadState {
    match state {
        BrowserWaitForLoadStateState::DomContentLoaded => {
            synapse_a11y::CdpLoadState::DomContentLoaded
        }
        BrowserWaitForLoadStateState::Load => synapse_a11y::CdpLoadState::Load,
        BrowserWaitForLoadStateState::NetworkIdle => synapse_a11y::CdpLoadState::NetworkIdle,
    }
}

fn browser_wait_for_state_bridge_name(state: BrowserWaitForState) -> &'static str {
    match state {
        BrowserWaitForState::TextAppears => "text_appears",
        BrowserWaitForState::TextGone => "text_gone",
        BrowserWaitForState::Timeout => "timeout",
    }
}

fn browser_wait_for_load_state_bridge_name(state: BrowserWaitForLoadStateState) -> &'static str {
    match state {
        BrowserWaitForLoadStateState::DomContentLoaded => "domcontentloaded",
        BrowserWaitForLoadStateState::Load => "load",
        BrowserWaitForLoadStateState::NetworkIdle => "networkidle",
    }
}

fn browser_wait_for_selector_state_bridge_name(state: BrowserWaitForSelectorState) -> &'static str {
    match state {
        BrowserWaitForSelectorState::Attached => "attached",
        BrowserWaitForSelectorState::Visible => "visible",
        BrowserWaitForSelectorState::Hidden => "hidden",
        BrowserWaitForSelectorState::Detached => "detached",
    }
}

#[cfg(windows)]
fn browser_wait_for_url_match_kind_to_a11y(
    kind: BrowserWaitForUrlMatchKind,
) -> synapse_a11y::CdpUrlMatchKind {
    match kind {
        BrowserWaitForUrlMatchKind::Exact => synapse_a11y::CdpUrlMatchKind::Exact,
        BrowserWaitForUrlMatchKind::Glob => synapse_a11y::CdpUrlMatchKind::Glob,
        BrowserWaitForUrlMatchKind::Regex => synapse_a11y::CdpUrlMatchKind::Regex,
    }
}

fn browser_wait_for_url_match_kind_bridge_name(kind: BrowserWaitForUrlMatchKind) -> &'static str {
    match kind {
        BrowserWaitForUrlMatchKind::Exact => "exact",
        BrowserWaitForUrlMatchKind::Glob => "glob",
        BrowserWaitForUrlMatchKind::Regex => "regex",
    }
}

#[cfg(windows)]
fn browser_locate_cdp_request(
    params: &BrowserLocateParams,
    root_backend_node_id: Option<i64>,
    limit: usize,
) -> synapse_a11y::CdpLocateRequest {
    synapse_a11y::CdpLocateRequest {
        engine: browser_locate_engine_to_a11y(params.engine),
        query: params.query.clone(),
        exact: params.exact.unwrap_or(false),
        regex: params.regex.unwrap_or(false),
        name: params.name.clone(),
        name_exact: params.name_exact.unwrap_or(false),
        name_regex: params.name_regex.unwrap_or(false),
        testid_attribute: params.testid_attribute.clone(),
        checked: params.checked,
        pressed: params.pressed,
        expanded: params.expanded,
        selected: params.selected,
        disabled: params.disabled,
        level: params.level,
        include_hidden: params.include_hidden.unwrap_or(false),
        relation: params.relation.map(browser_layout_relation_to_a11y),
        anchor: params.anchor.clone(),
        max_distance: params.max_distance,
        has_text: params.has_text.clone(),
        nth: params.nth,
        strict: params.strict.unwrap_or(false),
        root_backend_node_id,
        frame_id: None,
        limit,
    }
}

#[cfg(windows)]
#[derive(Clone, Debug)]
struct BrowserLocateScope {
    cdp_target_id: String,
    frame_id: Option<String>,
    frame_readback: Option<BrowserLocatedFrame>,
    frame_requested: bool,
    frame_resolved: bool,
    page_url: Option<String>,
    page_title: Option<String>,
}

#[cfg(windows)]
async fn resolve_browser_locate_scope(
    endpoint: &str,
    window_hwnd: i64,
    cdp_target_id: &str,
    frame: Option<&BrowserFrameLocator>,
) -> Result<BrowserLocateScope, ErrorData> {
    let Some(frame) = frame else {
        return Ok(BrowserLocateScope {
            cdp_target_id: cdp_target_id.to_owned(),
            frame_id: None,
            frame_readback: None,
            frame_requested: false,
            frame_resolved: true,
            page_url: None,
            page_title: None,
        });
    };
    let frames = synapse_a11y::cdp_list_frames(endpoint, window_hwnd, cdp_target_id)
        .await
        .map_err(|error| {
            mcp_error(
                error.code(),
                format!("browser_locate frame locator enumeration failed: {error}"),
            )
        })?;
    let matches = matching_browser_frames(frame, &frames.frames);
    let selected = match matches.as_slice() {
        [] => {
            return Ok(BrowserLocateScope {
                cdp_target_id: cdp_target_id.to_owned(),
                frame_id: None,
                frame_readback: Some(BrowserLocatedFrame {
                    resolved: false,
                    matched_frame_count: 0,
                    frame_id: None,
                    parent_frame_id: None,
                    cdp_target_id: None,
                    url: None,
                    name: None,
                    origin: None,
                    is_out_of_process: false,
                    frame_element_id: frame.frame_element_id.clone(),
                    frame_element_cdp_target_id: None,
                    frame_element_source: "not_found".to_owned(),
                }),
                frame_requested: true,
                frame_resolved: false,
                page_url: Some(redact_url_for_public_readback(&frames.page_url)),
                page_title: Some(frames.page_title),
            });
        }
        [selected] => *selected,
        many => {
            return Err(mcp_error(
                error_codes::ACTION_TARGET_INVALID,
                format!(
                    "browser_locate frame locator matched {} frames; refine by frame_id, frame_element_id, or index",
                    many.len()
                ),
            ));
        }
    };
    let min_target_depth = frames
        .frames
        .iter()
        .filter(|frame| {
            frame
                .cdp_target_id
                .eq_ignore_ascii_case(&selected.cdp_target_id)
        })
        .map(|frame| frame.depth)
        .min()
        .unwrap_or(selected.depth);
    let frame_id = (selected.depth > min_target_depth).then(|| selected.frame_id.clone());
    Ok(BrowserLocateScope {
        cdp_target_id: selected.cdp_target_id.clone(),
        frame_id,
        frame_readback: Some(browser_located_frame(selected, matches.len())),
        frame_requested: true,
        frame_resolved: true,
        page_url: Some(redact_url_for_public_readback(&frames.page_url)),
        page_title: Some(frames.page_title),
    })
}

#[cfg(windows)]
fn matching_browser_frames<'a>(
    locator: &BrowserFrameLocator,
    frames: &'a [synapse_a11y::CdpFrameTreeEntry],
) -> Vec<&'a synapse_a11y::CdpFrameTreeEntry> {
    if let Some(index) = locator.index {
        return frames.get(index).into_iter().collect();
    }
    if let Some(frame_id) = trimmed_frame_locator_value(locator.frame_id.as_deref()) {
        return frames
            .iter()
            .filter(|frame| frame.frame_id == frame_id)
            .collect();
    }
    if let Some(frame_element_id) = trimmed_frame_locator_value(locator.frame_element_id.as_deref())
    {
        return frames
            .iter()
            .filter(|frame| frame.frame_element_id.as_deref() == Some(frame_element_id.as_str()))
            .collect();
    }
    if let Some(name) = trimmed_frame_locator_value(locator.name.as_deref()) {
        return frames
            .iter()
            .filter(|frame| frame.name.as_deref() == Some(name.as_str()))
            .collect();
    }
    if let Some(url) = trimmed_frame_locator_value(locator.url.as_deref()) {
        return frames.iter().filter(|frame| frame.url == url).collect();
    }
    Vec::new()
}

#[cfg(windows)]
fn trimmed_frame_locator_value(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

#[cfg(windows)]
fn browser_located_frame(
    frame: &synapse_a11y::CdpFrameTreeEntry,
    matched_frame_count: usize,
) -> BrowserLocatedFrame {
    BrowserLocatedFrame {
        resolved: true,
        matched_frame_count,
        frame_id: Some(frame.frame_id.clone()),
        parent_frame_id: frame.parent_frame_id.clone(),
        cdp_target_id: Some(frame.cdp_target_id.clone()),
        url: Some(redact_url_for_public_readback(&frame.url)),
        name: frame.name.clone(),
        origin: Some(frame.origin.clone()),
        is_out_of_process: frame.is_out_of_process,
        frame_element_id: frame.frame_element_id.clone(),
        frame_element_cdp_target_id: frame.frame_element_cdp_target_id.clone(),
        frame_element_source: frame.frame_element_source.clone(),
    }
}

#[cfg(windows)]
fn browser_chrome_bridge_located_frame(
    frame: crate::chrome_debugger_bridge::ChromeDebuggerLocatedFrame,
) -> BrowserLocatedFrame {
    BrowserLocatedFrame {
        resolved: frame.resolved,
        matched_frame_count: frame.matched_frame_count,
        frame_id: frame.frame_id,
        parent_frame_id: frame.parent_frame_id,
        cdp_target_id: frame.cdp_target_id,
        url: redact_url_opt_for_public_readback(frame.url),
        name: frame.name,
        origin: frame.origin,
        is_out_of_process: frame.is_out_of_process,
        frame_element_id: frame.frame_element_id,
        frame_element_cdp_target_id: frame.frame_element_cdp_target_id,
        frame_element_source: frame.frame_element_source,
    }
}

/// Maps the MCP layout relation onto the a11y layout relation.
#[cfg(windows)]
fn browser_layout_relation_to_a11y(
    relation: BrowserLayoutRelation,
) -> synapse_a11y::CdpLayoutRelation {
    match relation {
        BrowserLayoutRelation::Near => synapse_a11y::CdpLayoutRelation::Near,
        BrowserLayoutRelation::RightOf => synapse_a11y::CdpLayoutRelation::RightOf,
        BrowserLayoutRelation::LeftOf => synapse_a11y::CdpLayoutRelation::LeftOf,
        BrowserLayoutRelation::Above => synapse_a11y::CdpLayoutRelation::Above,
        BrowserLayoutRelation::Below => synapse_a11y::CdpLayoutRelation::Below,
    }
}

const DEFAULT_BROWSER_CONTENT_MAX_BYTES: usize = 2 * 1024 * 1024;
const MAX_BROWSER_CONTENT_BYTES: usize = 8 * 1024 * 1024;
const MAX_BROWSER_SET_CONTENT_HTML_BYTES: usize = 8 * 1024 * 1024;
const DEFAULT_BROWSER_INSPECT_HTML_BYTES: usize = 256 * 1024;
const MAX_BROWSER_INSPECT_HTML_BYTES: usize = 4 * 1024 * 1024;

fn validate_browser_set_content_params(params: &BrowserSetContentParams) -> Result<(), ErrorData> {
    if params.html.trim().is_empty() {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "browser_set_content requires non-empty html",
        ));
    }
    if params.html.len() > MAX_BROWSER_SET_CONTENT_HTML_BYTES {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "browser_set_content html is {} bytes; the maximum is {MAX_BROWSER_SET_CONTENT_HTML_BYTES} bytes",
                params.html.len()
            ),
        ));
    }
    if let Some(target_id) = params.cdp_target_id.as_deref() {
        validate_cdp_target_id(target_id)?;
    }
    Ok(())
}

/// In-page payload returned by `browser_content`'s evaluation.
#[cfg(windows)]
#[derive(serde::Deserialize)]
struct BrowserContentPayload {
    html: String,
    html_len: usize,
    truncated: bool,
}

/// Shared error for browser introspection tools when the owned target has no
/// raw CDP debugging endpoint (the popup-safe extension bridge cannot serve it).
#[cfg(windows)]
pub(super) fn browser_raw_cdp_required_error(tool: &str, window_hwnd: i64) -> ErrorData {
    mcp_error(
        error_codes::A11Y_CDP_EXTENSION_UNAVAILABLE,
        format!(
            "{tool} requires a raw CDP debugging endpoint for window {window_hwnd:#x}; the popup-safe normal Chrome extension bridge does not expose page/element introspection over CDP (it never attaches the debugger). Open the target in a raw-CDP Chrome (launched with --remote-debugging-port) and retry."
        ),
    )
}

/// Element-scoped introspection function (called Playwright-style as
/// `fn(element, maxBytes)`). Returns the typed [`super::ElementInspection`]
/// payload computed entirely in-page in a single round trip.
#[cfg(windows)]
const BROWSER_INSPECT_FUNCTION: &str = r#"(el, maxBytes) => {
    const max = (typeof maxBytes === "number" && maxBytes >= 0) ? maxBytes : 0;
    const str = v => String(v == null ? "" : v);
    const outer = str(el.outerHTML);
    const inner = str(el.innerHTML);
    const innerText = str(el.innerText);
    const textContent = str(el.textContent);
    const truncated = outer.length > max || inner.length > max || innerText.length > max || textContent.length > max;
    const attrs = {};
    if (el.attributes) { for (const a of el.attributes) { attrs[a.name] = a.value; } }
    const rect = el.getBoundingClientRect ? el.getBoundingClientRect() : { left: 0, top: 0, width: 0, height: 0 };
    const cs = (el.nodeType === 1 && el.ownerDocument && el.ownerDocument.defaultView)
        ? el.ownerDocument.defaultView.getComputedStyle(el) : null;
    const hasLayout = !!(el.offsetWidth || el.offsetHeight || (el.getClientRects && el.getClientRects().length));
    const visible = hasLayout && (!cs || (cs.visibility !== "hidden" && cs.display !== "none" && cs.opacity !== "0"));
    const tag = str(el.tagName);
    const tagU = tag.toUpperCase();
    const getAttr = name => (el.getAttribute ? str(el.getAttribute(name)) : "");
    const inputType = (getAttr("type") || "text").toLowerCase();
    const textTypes = new Set(["text","search","url","tel","email","password","number","date","datetime-local","month","time","week","color"]);
    const ariaDisabled = getAttr("aria-disabled").toLowerCase() === "true";
    const enabled = !(("disabled" in el) ? !!el.disabled : false) && !ariaDisabled;
    const readOnly = ("readOnly" in el) ? !!el.readOnly : false;
    const editable = (tagU === "TEXTAREA" && enabled && !readOnly) ||
        (tagU === "INPUT" && textTypes.has(inputType) && enabled && !readOnly) ||
        (!!el.isContentEditable && enabled) ||
        (getAttr("role").toLowerCase() === "textbox" && enabled);
    let checked;
    if ("checked" in el) { checked = !!el.checked; }
    else { checked = getAttr("aria-checked").toLowerCase() === "true"; }
    const sx = (typeof window !== "undefined" && window.scrollX) || 0;
    const sy = (typeof window !== "undefined" && window.scrollY) || 0;
    return {
        tag_name: tag,
        outer_html: outer.slice(0, max),
        inner_html: inner.slice(0, max),
        inner_text: innerText.slice(0, max),
        text_content: textContent.slice(0, max),
        html_truncated: truncated,
        max_html_bytes: max,
        attributes: attrs,
        input_value: ("value" in el) ? str(el.value) : null,
        is_visible: visible,
        is_enabled: enabled,
        is_checked: checked,
        is_editable: editable,
        bounding_box: {
            x: rect.left + sx,
            y: rect.top + sy,
            viewport_x: rect.left,
            viewport_y: rect.top,
            width: rect.width,
            height: rect.height
        },
        device_pixel_ratio: (typeof window !== "undefined" && window.devicePixelRatio) || 1
    };
}"#;

#[cfg(windows)]
const BROWSER_WAIT_FOR_SELECTOR_STATE_FUNCTION: &str = r#"(el) => {
    const connected = !!(el && el.isConnected);
    const rect = el && el.getBoundingClientRect ? el.getBoundingClientRect() : { width: 0, height: 0 };
    const cs = (el && el.nodeType === 1 && el.ownerDocument && el.ownerDocument.defaultView)
        ? el.ownerDocument.defaultView.getComputedStyle(el) : null;
    const hasLayout = !!(el && (el.offsetWidth || el.offsetHeight || (el.getClientRects && el.getClientRects().length)));
    const visible = connected && hasLayout && rect.width >= 0 && rect.height >= 0 &&
        (!cs || (cs.visibility !== "hidden" && cs.display !== "none" && cs.opacity !== "0"));
    return { is_connected: connected, is_visible: visible };
}"#;

#[cfg(windows)]
async fn browser_wait_for_selector_poll(
    endpoint: &str,
    window_hwnd: i64,
    cdp_target_id: &str,
    wait: &NormalizedBrowserWaitForSelectorParams,
    root_backend_node_id: Option<i64>,
) -> Result<BrowserWaitForSelectorPoll, ErrorData> {
    let scope = resolve_browser_locate_scope(
        endpoint,
        window_hwnd,
        cdp_target_id,
        wait.locate.frame.as_ref(),
    )
    .await?;
    if scope.frame_requested && !scope.frame_resolved {
        let observation = BrowserWaitForSelectorObservation::default();
        let (condition_met, _) = browser_wait_for_selector_condition(wait.state, &observation);
        return Ok(BrowserWaitForSelectorPoll {
            condition_met,
            cdp_target_id: cdp_target_id.to_owned(),
            engine: browser_locate_engine_to_a11y(wait.locate.engine)
                .as_str()
                .to_owned(),
            query: wait.locate.query.clone(),
            match_count: 0,
            returned_count: 0,
            visible_count: 0,
            truncated: false,
            element_id: None,
            frame: scope.frame_readback,
            url: scope
                .page_url
                .map(|url| redact_url_for_public_readback(&url))
                .unwrap_or_default(),
            title: scope.page_title.unwrap_or_default(),
        });
    }

    let mut request = browser_locate_cdp_request(&wait.locate, root_backend_node_id, wait.limit);
    request.frame_id = scope.frame_id.clone();
    let located = synapse_a11y::cdp_locate(endpoint, &scope.cdp_target_id, request)
        .await
        .map_err(|error| {
            mcp_error(
                error.code(),
                format!("browser_wait_for_selector raw CDP selector resolution failed: {error}"),
            )
        })?;
    let mut observation = BrowserWaitForSelectorObservation {
        returned_backend_node_ids: located.backend_node_ids.clone(),
        truncated: located.truncated,
        ..Default::default()
    };
    if matches!(
        wait.state,
        BrowserWaitForSelectorState::Visible | BrowserWaitForSelectorState::Hidden
    ) {
        for backend_node_id in &located.backend_node_ids {
            if browser_wait_for_selector_backend_visible(
                endpoint,
                &located.target_id,
                *backend_node_id,
            )
            .await?
            {
                observation.visible_backend_node_ids.push(*backend_node_id);
            } else {
                observation.hidden_backend_node_ids.push(*backend_node_id);
            }
        }
    }
    let (condition_met, satisfied_backend_node_id) =
        browser_wait_for_selector_condition(wait.state, &observation);
    let element_id = satisfied_backend_node_id.map(|backend| {
        synapse_a11y::cdp_element_id_for_target(window_hwnd, &located.target_id, backend)
            .to_string()
    });
    Ok(BrowserWaitForSelectorPoll {
        condition_met,
        cdp_target_id: located.target_id,
        engine: located.engine,
        query: located.query,
        match_count: located.match_count,
        returned_count: located.returned_count,
        visible_count: observation.visible_backend_node_ids.len(),
        truncated: located.truncated,
        element_id,
        frame: scope.frame_readback,
        url: redact_url_for_public_readback(&located.url),
        title: located.title,
    })
}

#[cfg(windows)]
async fn browser_wait_for_selector_backend_visible(
    endpoint: &str,
    cdp_target_id: &str,
    backend_node_id: i64,
) -> Result<bool, ErrorData> {
    let evaluated = match synapse_a11y::cdp_evaluate_on_element(
        endpoint,
        cdp_target_id,
        backend_node_id,
        BROWSER_WAIT_FOR_SELECTOR_STATE_FUNCTION,
        &[],
        true,
        true,
    )
    .await
    {
        Ok(evaluated) => evaluated,
        Err(error) => {
            let detail = error.to_string();
            if detail.contains("returned no objectId")
                || detail.contains("not present")
                || detail.contains("detached")
            {
                return Ok(false);
            }
            return Err(mcp_error(
                error.code(),
                format!(
                    "browser_wait_for_selector element visibility readback failed for backendNodeId {backend_node_id}: {error}"
                ),
            ));
        }
    };
    let state: BrowserWaitForSelectorElementState = serde_json::from_value(evaluated.value)
        .map_err(|error| {
            mcp_error(
                error_codes::OBSERVE_INTERNAL,
                format!(
                    "browser_wait_for_selector visibility payload decode failed for backendNodeId {backend_node_id}: {error}"
                ),
            )
        })?;
    Ok(state.is_connected && state.is_visible)
}

fn browser_wait_for_selector_condition(
    state: BrowserWaitForSelectorState,
    observation: &BrowserWaitForSelectorObservation,
) -> (bool, Option<i64>) {
    let first_returned = observation.returned_backend_node_ids.first().copied();
    let first_visible = observation.visible_backend_node_ids.first().copied();
    let first_hidden = observation.hidden_backend_node_ids.first().copied();
    match state {
        BrowserWaitForSelectorState::Attached => (first_returned.is_some(), first_returned),
        BrowserWaitForSelectorState::Visible => (first_visible.is_some(), first_visible),
        BrowserWaitForSelectorState::Hidden => {
            if first_returned.is_none() {
                (true, None)
            } else if observation.visible_backend_node_ids.is_empty() && !observation.truncated {
                (true, first_hidden.or(first_returned))
            } else {
                (false, None)
            }
        }
        BrowserWaitForSelectorState::Detached => (first_returned.is_none(), None),
    }
}

#[cfg(windows)]
fn duration_millis_u64(duration: std::time::Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// Parses a browser `element_id` into its `(backendNodeId, cdp_target_id)` for
/// element-scoped evaluation, failing loud when it is not a CDP web element.
fn parse_browser_evaluate_element(element_id: &str) -> Result<(i64, String), ErrorData> {
    let parsed = synapse_core::ElementId::parse(element_id).map_err(|err| {
        mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("browser_evaluate element_id {element_id:?} is not a valid element id: {err}"),
        )
    })?;
    let backend = synapse_a11y::cdp_backend_from_element_id(&parsed).ok_or_else(|| {
        mcp_error(
            error_codes::ACTION_TARGET_INVALID,
            format!(
                "browser_evaluate element_id {element_id:?} is not a CDP web element (no backendNodeId); element-scoped evaluation only supports browser DOM elements"
            ),
        )
    })?;
    let target = synapse_a11y::cdp_target_from_element_id(&parsed).ok_or_else(|| {
        mcp_error(
            error_codes::ACTION_TARGET_INVALID,
            format!(
                "browser_evaluate element_id {element_id:?} has no embedded CDP target id; re-resolve it via find/observe against the owned tab"
            ),
        )
    })?;
    Ok((backend, target))
}

fn parse_chrome_bridge_element_target(element_id: &str) -> Result<Option<String>, ErrorData> {
    let trimmed = element_id.trim();
    let Some(after_prefix) = trimmed.strip_prefix("chrome-tab:") else {
        return Ok(None);
    };
    let Some((tab_id, after_frame_marker)) = after_prefix.split_once(":frame:") else {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "normal Chrome bridge element_id {element_id:?} must be shaped like chrome-tab:<tabId>:frame:<frameId>:path:<domPath>"
            ),
        ));
    };
    let Some((frame_id, path)) = after_frame_marker.split_once(":path:") else {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("normal Chrome bridge element_id {element_id:?} must include :path:<domPath>"),
        ));
    };
    if tab_id.is_empty()
        || !tab_id.bytes().all(|byte| byte.is_ascii_digit())
        || frame_id.is_empty()
        || !frame_id.bytes().all(|byte| byte.is_ascii_digit())
        || path.is_empty()
        || !path.split('.').all(|part| {
            // Numeric child index, or the "s" shadow-host-hop token (#1335): the
            // bridge encodes open-shadow-root crossings as a literal "s" segment
            // (e.g. 0.1.1.s.0). Light-DOM paths remain all-numeric.
            !part.is_empty() && (part == "s" || part.bytes().all(|byte| byte.is_ascii_digit()))
        })
    {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "normal Chrome bridge element_id {element_id:?} has invalid tab/frame/path components"
            ),
        ));
    }
    Ok(Some(format!("chrome-tab:{tab_id}")))
}

const DEFAULT_CDP_NAVIGATE_WAIT_TIMEOUT_MS: u64 = 10_000;
const MAX_CDP_NAVIGATE_WAIT_TIMEOUT_MS: u64 = 30_000;

#[derive(Debug)]
struct BrowserNavValidated {
    params: BrowserNavParams,
    action: CdpNavigateAction,
    requested_url: Option<String>,
    wait_timeout_ms: u64,
    ignore_cache: bool,
}

fn validate_browser_nav_params(params: BrowserNavParams) -> Result<BrowserNavValidated, ErrorData> {
    let action = browser_nav_action(params.operation);
    let requested_url = match params.operation {
        BrowserNavOperation::Navigate => {
            let Some(url) = params.url.as_deref() else {
                return Err(browser_nav_facade_error(
                    params.operation,
                    browser_nav_source_id(&params),
                    "browser_nav operation=navigate requires url",
                    "provide a non-empty URL for navigate or choose reload/back/forward without url",
                ));
            };
            validate_browser_nav_url(params.operation, url)?;
            Some(url.to_owned())
        }
        BrowserNavOperation::Reload | BrowserNavOperation::Back | BrowserNavOperation::Forward => {
            if params.url.is_some() {
                return Err(browser_nav_facade_error(
                    params.operation,
                    browser_nav_source_id(&params),
                    format!(
                        "browser_nav operation={} does not accept url",
                        params.operation.as_str()
                    ),
                    "remove url unless operation=navigate",
                ));
            }
            None
        }
    };
    if params.ignore_cache.is_some() && params.operation != BrowserNavOperation::Reload {
        return Err(browser_nav_facade_error(
            params.operation,
            browser_nav_source_id(&params),
            format!(
                "browser_nav operation={} does not accept ignore_cache",
                params.operation.as_str()
            ),
            "use ignore_cache only with operation=reload",
        ));
    }
    let wait_timeout_ms = params
        .wait_timeout_ms
        .unwrap_or(DEFAULT_CDP_NAVIGATE_WAIT_TIMEOUT_MS);
    if wait_timeout_ms == 0 || wait_timeout_ms > MAX_CDP_NAVIGATE_WAIT_TIMEOUT_MS {
        return Err(browser_nav_facade_error(
            params.operation,
            browser_nav_source_id(&params),
            format!("browser_nav wait_timeout_ms must be 1..={MAX_CDP_NAVIGATE_WAIT_TIMEOUT_MS}"),
            "use a positive wait_timeout_ms at or below the navigation readback cap",
        ));
    }
    Ok(BrowserNavValidated {
        action,
        requested_url,
        wait_timeout_ms,
        ignore_cache: params.ignore_cache.unwrap_or(false),
        params,
    })
}

fn browser_nav_action(operation: BrowserNavOperation) -> CdpNavigateAction {
    match operation {
        BrowserNavOperation::Navigate => CdpNavigateAction::Navigate,
        BrowserNavOperation::Reload => CdpNavigateAction::Reload,
        BrowserNavOperation::Back => CdpNavigateAction::Back,
        BrowserNavOperation::Forward => CdpNavigateAction::Forward,
    }
}

fn cdp_navigation_error_code(code: &'static str) -> &'static str {
    if code == error_codes::A11Y_CDP_AXTREE_FAILED {
        error_codes::BROWSER_NAVIGATION_FAILED
    } else {
        code
    }
}

fn validate_browser_nav_url(operation: BrowserNavOperation, url: &str) -> Result<(), ErrorData> {
    if url.is_empty() {
        return Err(browser_nav_facade_error(
            operation,
            "url",
            "browser_nav url must not be empty",
            "provide an absolute URL or use browser_tabs operation=new with an empty string for about:blank",
        ));
    }
    if url.chars().count() > 8192 {
        return Err(browser_nav_facade_error(
            operation,
            "url",
            "browser_nav url must be at most 8192 Unicode scalar values",
            "shorten the URL before passing it to browser_nav",
        ));
    }
    if url.contains('\0') {
        return Err(browser_nav_facade_error(
            operation,
            "url",
            "browser_nav url must not contain NUL",
            "remove NUL bytes from the URL",
        ));
    }
    if url.trim() != url {
        return Err(browser_nav_facade_error(
            operation,
            "url",
            "browser_nav url must not contain leading or trailing whitespace",
            "trim the URL before passing it to browser_nav",
        ));
    }
    if reqwest::Url::parse(url).is_err() {
        return Err(browser_nav_facade_error(
            operation,
            "url",
            "browser_nav url must be an absolute URL",
            "provide an absolute URL with a scheme; use browser_tabs operation=new with an empty string for about:blank",
        ));
    }
    Ok(())
}

fn browser_nav_source_id(params: &BrowserNavParams) -> String {
    match (params.window_hwnd, params.cdp_target_id.as_deref()) {
        (Some(hwnd), Some(target)) => format!("window_hwnd={hwnd:#x};cdp_target_id={target}"),
        (Some(hwnd), None) => format!("window_hwnd={hwnd:#x}"),
        (None, Some(target)) => format!("cdp_target_id={target}"),
        (None, None) => "active_session_target".to_owned(),
    }
}

fn browser_nav_facade_error(
    operation: BrowserNavOperation,
    source_id: impl Into<String>,
    message: impl Into<String>,
    remediation: &'static str,
) -> ErrorData {
    let message = message.into();
    ErrorData::new(
        ErrorCode(-32099),
        message,
        Some(json!({
            "code": error_codes::TOOL_PARAMS_INVALID,
            "operation": operation.as_str(),
            "source_of_truth": BROWSER_NAV_SOURCE_OF_TRUTH,
            "source_id": source_id.into(),
            "readback_source_of_truth": BROWSER_NAV_READBACK_SOURCE_OF_TRUTH,
            "remediation": remediation,
        })),
    )
}

fn browser_nav_delegate_error(
    operation: BrowserNavOperation,
    source_id: impl Into<String>,
    error: ErrorData,
    remediation: &'static str,
) -> ErrorData {
    let cause_code = error
        .data
        .as_ref()
        .and_then(|data| data.get("code"))
        .and_then(Value::as_str)
        .unwrap_or(error_codes::TOOL_INTERNAL_ERROR)
        .to_owned();
    let cause_data = error.data.clone().unwrap_or(Value::Null);
    ErrorData::new(
        error.code,
        error.message.to_string(),
        Some(json!({
            "code": cause_code,
            "operation": operation.as_str(),
            "source_of_truth": BROWSER_NAV_SOURCE_OF_TRUTH,
            "source_id": source_id.into(),
            "readback_source_of_truth": BROWSER_NAV_READBACK_SOURCE_OF_TRUTH,
            "remediation": remediation,
            "cause": cause_data,
        })),
    )
}

fn validate_cdp_navigation_params(
    params: &CdpNavigateTabParams,
) -> Result<Option<String>, ErrorData> {
    match params.action {
        CdpNavigateAction::Navigate => {
            let Some(url) = params.url.as_deref() else {
                return Err(mcp_error(
                    error_codes::TOOL_PARAMS_INVALID,
                    "cdp_navigate_tab action=navigate requires url",
                ));
            };
            validate_cdp_navigation_url(url)?;
            Ok(Some(url.to_owned()))
        }
        CdpNavigateAction::Reload | CdpNavigateAction::Back | CdpNavigateAction::Forward => {
            if params.url.is_some() {
                return Err(mcp_error(
                    error_codes::TOOL_PARAMS_INVALID,
                    "cdp_navigate_tab url is only valid with action=navigate",
                ));
            }
            Ok(None)
        }
    }
}

fn validate_cdp_navigation_wait_timeout(value: Option<u64>) -> Result<u64, ErrorData> {
    let value = value.unwrap_or(DEFAULT_CDP_NAVIGATE_WAIT_TIMEOUT_MS);
    if value == 0 || value > MAX_CDP_NAVIGATE_WAIT_TIMEOUT_MS {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "cdp_navigate_tab wait_timeout_ms must be 1..={MAX_CDP_NAVIGATE_WAIT_TIMEOUT_MS}"
            ),
        ));
    }
    Ok(value)
}

fn validate_cdp_navigation_url(url: &str) -> Result<(), ErrorData> {
    if url.is_empty() {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "cdp_navigate_tab url must not be empty",
        ));
    }
    if url.chars().count() > 8192 {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "cdp_navigate_tab url must be at most 8192 Unicode scalar values",
        ));
    }
    if url.contains('\0') {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "cdp_navigate_tab url must not contain NUL",
        ));
    }
    if url.trim() != url {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "cdp_navigate_tab url must not contain leading or trailing whitespace",
        ));
    }
    if reqwest::Url::parse(url).is_err() {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "cdp_navigate_tab url must be an absolute URL",
        ));
    }
    Ok(())
}

fn validate_cdp_tab_url(url: &str) -> Result<(), ErrorData> {
    if url.chars().count() > 8192 {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "cdp_open_tab url must be at most 8192 Unicode scalar values",
        ));
    }
    if url.contains('\0') {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "cdp_open_tab url must not contain NUL",
        ));
    }
    if !url.is_empty() && url.trim() != url {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "cdp_open_tab url must not contain leading or trailing whitespace; use an empty string for about:blank",
        ));
    }
    if !url.is_empty() && reqwest::Url::parse(url).is_err() {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "cdp_open_tab url must be an absolute URL; use an empty string for about:blank",
        ));
    }
    Ok(())
}

#[cfg(windows)]
fn cdp_endpoint_for_action_log(window_hwnd: i64) -> String {
    synapse_a11y::endpoint_for_window(window_hwnd).unwrap_or_else(chrome_debugger_default_endpoint)
}

#[cfg(not(windows))]
fn cdp_endpoint_for_action_log(_window_hwnd: i64) -> String {
    chrome_debugger_default_endpoint()
}

fn is_chrome_debugger_endpoint(endpoint: &str) -> bool {
    endpoint.starts_with("chrome-extension://")
        && (endpoint.ends_with("/chrome.tabs") || endpoint.ends_with("/chrome.debugger"))
}

fn unix_ms_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        })
}

#[cfg(windows)]
fn current_human_os_foreground_hwnd() -> Option<i64> {
    synapse_a11y::current_foreground_context()
        .ok()
        .map(|context| context.hwnd)
}

fn background_tab_activation_foregrounded_requested_window(
    before_hwnd: Option<i64>,
    after_hwnd: Option<i64>,
    requested_window_hwnd: i64,
) -> bool {
    before_hwnd != Some(requested_window_hwnd)
        && after_hwnd == Some(requested_window_hwnd)
        && after_hwnd != before_hwnd
}

#[cfg(windows)]
fn passive_chromium_window_candidates(
    tool: &str,
    foreground: Option<&ForegroundContext>,
) -> Result<Vec<ForegroundContext>, ErrorData> {
    let contexts = synapse_a11y::visible_top_level_window_contexts().map_err(|error| {
        mcp_error(
            error.code(),
            format!("{tool} could not enumerate visible Chromium windows: {error}"),
        )
    })?;
    let mut candidates = contexts
        .into_iter()
        .filter(|context| synapse_a11y::is_chromium_family(&context.process_name))
        .collect::<Vec<_>>();
    candidates.sort_by_key(|context| {
        (
            foreground.is_some_and(|fg| fg.hwnd == context.hwnd),
            context.process_name.to_ascii_lowercase(),
            context.window_title.to_ascii_lowercase(),
            context.hwnd,
        )
    });
    Ok(candidates)
}

#[cfg(windows)]
fn format_chromium_window_candidates(candidates: &[ForegroundContext]) -> String {
    candidates
        .iter()
        .take(8)
        .map(|context| {
            format!(
                "hwnd={:#x} pid={} process={:?} title={:?} bounds={}x{}+{},{}",
                context.hwnd,
                context.pid,
                context.process_name,
                context.window_title,
                context.window_bounds.w,
                context.window_bounds.h,
                context.window_bounds.x,
                context.window_bounds.y
            )
        })
        .collect::<Vec<_>>()
        .join("; ")
}

#[cfg(windows)]
fn browser_tabs_activation_visual_probe(
    window_hwnd: i64,
    phase: &str,
) -> Result<BrowserTabsActivationVisualProbe, ErrorData> {
    let minimized = synapse_a11y::is_window_minimized(window_hwnd).map_err(|error| {
        mcp_error(
            error.code(),
            format!(
                "browser_tabs operation=activate could not read minimized state for Chrome HWND {window_hwnd:#x} before visual postcondition: {error}"
            ),
        )
    })?;
    if minimized {
        return Err(mcp_error(
            error_codes::ACTION_POSTCONDITION_FAILED,
            format!(
                "browser_tabs operation=activate cannot prove visible HWND pixels for minimized Chrome HWND {window_hwnd:#x}; restore the window or use target-scoped browser_capture instead of HWND capture"
            ),
        ));
    }
    let context = validate_target_window_context(window_hwnd)?;
    let captured =
        synapse_capture::window_full_frame_to_bgra_bitmap(window_hwnd, WINDOW_SCREENSHOT_TIMEOUT_MS)
            .map_err(|error| {
                mcp_error(
                    error.code(),
                    format!(
                        "browser_tabs operation=activate passive HWND visual readback failed during {phase} for Chrome HWND {window_hwnd:#x}: {error}"
                    ),
                )
            })?;
    let bitmap = captured.bitmap;
    let bitmap_sha256 = sha256_hex(&bitmap.bytes);
    tracing::info!(
        code = "BROWSER_TABS_ACTIVATE_VISUAL_PROBE",
        hwnd = window_hwnd,
        phase,
        window_title = %context.window_title,
        width = bitmap.width,
        height = bitmap.height,
        bitmap_sha256 = %bitmap_sha256,
        "readback=passive_wgc_window_bgra outcome=activation_visual_probe"
    );
    Ok(BrowserTabsActivationVisualProbe {
        window_title: context.window_title,
        bitmap_sha256,
        width: bitmap.width,
        height: bitmap.height,
    })
}

#[cfg(windows)]
async fn browser_tabs_verify_activation_visualized(
    window_hwnd: i64,
    before_tab: &BrowserTabEntry,
    activated_tab: &BrowserTabEntry,
    visual_before: Option<&BrowserTabsActivationVisualProbe>,
) -> Result<BrowserTabsActivationVisualReadback, ErrorData> {
    let before = visual_before.ok_or_else(|| {
        mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            "browser_tabs operation=activate missing pre-activation visual readback for inactive tab"
                .to_owned(),
        )
    })?;
    let started = Instant::now();
    let mut attempts = 0_u32;
    loop {
        attempts = attempts.saturating_add(1);
        let after = browser_tabs_activation_visual_probe(window_hwnd, "after_activation")?;
        let visual_changed = before.bitmap_sha256 != after.bitmap_sha256;
        let title_match =
            browser_tab_window_title_matches_target(&after.window_title, activated_tab);
        if visual_changed && title_match.unwrap_or(true) {
            let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
            tracing::info!(
                code = "BROWSER_TABS_ACTIVATE_VISUAL_VERIFIED",
                hwnd = window_hwnd,
                requested_cdp_target_id = %activated_tab.cdp_target_id,
                before_cdp_target_id = %before_tab.cdp_target_id,
                attempts,
                elapsed_ms,
                before_bitmap_sha256 = %before.bitmap_sha256,
                after_bitmap_sha256 = %after.bitmap_sha256,
                target_title_matched_window_title = title_match.unwrap_or(true),
                "readback=passive_wgc_window_bgra outcome=activation_visual_postcondition_verified"
            );
            return Ok(browser_tabs_activation_visual_readback(
                "verified_hwnd_pixels_changed",
                before,
                &after,
                true,
                title_match,
                attempts,
                elapsed_ms,
            ));
        }
        if attempts >= 8 {
            let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
            tracing::error!(
                code = error_codes::ACTION_POSTCONDITION_FAILED,
                hwnd = window_hwnd,
                requested_cdp_target_id = %activated_tab.cdp_target_id,
                before_cdp_target_id = %before_tab.cdp_target_id,
                attempts,
                elapsed_ms,
                before_window_title = %before.window_title,
                after_window_title = %after.window_title,
                before_bitmap_sha256 = %before.bitmap_sha256,
                after_bitmap_sha256 = %after.bitmap_sha256,
                visual_changed = before.bitmap_sha256 != after.bitmap_sha256,
                target_title_matched_window_title = title_match.unwrap_or(true),
                "browser_tabs operation=activate Chrome tab state changed but passive HWND visual postcondition did not verify"
            );
            return Err(mcp_error(
                error_codes::ACTION_POSTCONDITION_FAILED,
                format!(
                    "browser_tabs operation=activate postcondition failed: Chrome reported target {:?} active, but passive HWND pixels for Chrome HWND {window_hwnd:#x} did not prove the visual tab switch after {attempts} attempts over {elapsed_ms} ms. before_target={:?} before_title={:?} after_title={:?} before_sha256={} after_sha256={} visual_changed={} title_matches_target={:?}. remediation=use target-scoped browser_capture for tab content or retry after Chrome repaints; HWND capture was not accepted as current.",
                    activated_tab.cdp_target_id,
                    before_tab.cdp_target_id,
                    before.window_title,
                    after.window_title,
                    before.bitmap_sha256,
                    after.bitmap_sha256,
                    before.bitmap_sha256 != after.bitmap_sha256,
                    title_match
                ),
            ));
        }
        tokio::time::sleep(std::time::Duration::from_millis(125)).await;
    }
}

#[cfg(windows)]
fn browser_tabs_activation_visual_readback(
    status: &str,
    before: &BrowserTabsActivationVisualProbe,
    after: &BrowserTabsActivationVisualProbe,
    visual_changed: bool,
    title_match: Option<bool>,
    attempts: u32,
    elapsed_ms: u64,
) -> BrowserTabsActivationVisualReadback {
    BrowserTabsActivationVisualReadback {
        status: status.to_owned(),
        source_of_truth:
            "passive per-window WGC BGRA capture of the owning Chrome HWND before and after chrome.tabs.update(active=true)"
                .to_owned(),
        before_window_title: Some(before.window_title.clone()),
        after_window_title: Some(after.window_title.clone()),
        before_bitmap_sha256: Some(before.bitmap_sha256.clone()),
        after_bitmap_sha256: Some(after.bitmap_sha256.clone()),
        before_bitmap_width: before.width,
        before_bitmap_height: before.height,
        after_bitmap_width: after.width,
        after_bitmap_height: after.height,
        visual_changed,
        target_title_matched_window_title: title_match,
        attempts,
        elapsed_ms,
    }
}

#[cfg(windows)]
fn browser_tab_window_title_matches_target(
    window_title: &str,
    activated_tab: &BrowserTabEntry,
) -> Option<bool> {
    let tab_title = activated_tab.title.trim();
    if tab_title.is_empty() || tab_title.eq_ignore_ascii_case("redacted") {
        return None;
    }
    let window = window_title.trim().to_ascii_lowercase();
    if window.is_empty() {
        return Some(false);
    }
    let tab = tab_title.to_ascii_lowercase();
    let window_without_suffix = window
        .strip_suffix(" - google chrome")
        .unwrap_or(&window)
        .trim();
    Some(
        window.contains(&tab)
            || tab.contains(window_without_suffix)
            || window_without_suffix.contains(&tab),
    )
}

#[derive(Clone, Debug, Default)]
struct BrowserScreenshotForegroundReadback {
    required_foreground: bool,
    before_hwnd: Option<i64>,
    capture_hwnd: Option<i64>,
    after_restore_hwnd: Option<i64>,
    restored_human_os_foreground: bool,
}

/// #1359: process-wide serialization of browser_screenshot's foreground-capture
/// critical section. Concurrent captures otherwise interleave their Chrome-window
/// activation/restore and one observes the other's foreground change as a
/// spurious drift, failing the capture. Held across prepare → capture → finish
/// (and the passive-window fallback), released on drop even on error.
static BROWSER_SCREENSHOT_FOREGROUND_LOCK: tokio::sync::Mutex<()> =
    tokio::sync::Mutex::const_new(());

#[cfg(windows)]
#[derive(Clone, Debug)]
struct BrowserScreenshotForegroundGuard {
    before: ForegroundContext,
    readback: BrowserScreenshotForegroundReadback,
}

#[cfg(not(windows))]
#[derive(Clone, Debug, Default)]
struct BrowserScreenshotForegroundGuard;

#[cfg(windows)]
fn prepare_browser_screenshot_foreground(
    window_hwnd: i64,
) -> Result<BrowserScreenshotForegroundGuard, ErrorData> {
    const TOOL: &str = "browser_screenshot";
    let before = read_browser_screenshot_current_foreground("before_capture")?;
    let target = synapse_a11y::foreground_context(window_hwnd).map_err(|error| {
        mcp_error(
            error_codes::TARGET_WINDOW_NOT_FOUND,
            format!(
                "browser_screenshot target HWND {window_hwnd:#x} is not inspectable before capture: {error}"
            ),
        )
    })?;
    let required_foreground = before.hwnd != window_hwnd;
    tracing::info!(
        code = "BROWSER_SCREENSHOT_FOREGROUND_PREFLIGHT",
        hwnd = window_hwnd,
        human_os_foreground_before_hwnd = before.hwnd,
        human_os_foreground_before_pid = before.pid,
        human_os_foreground_before_process = %before.process_name,
        human_os_foreground_before_title = %before.window_title,
        target_hwnd = target.hwnd,
        target_pid = target.pid,
        target_process = %target.process_name,
        target_title = %target.window_title,
        required_foreground,
        "readback=GetForegroundWindow outcome=foreground_precondition_evaluated"
    );

    if required_foreground {
        synapse_a11y::focus_window_with_intent(
            window_hwnd,
            synapse_a11y::ForegroundActivationIntent::OperatorRequested { caller: TOOL },
        )
        .map_err(|error| {
            mcp_error(
                error_codes::ACTION_LAUNCH_FOREGROUND_FAILED,
                format!(
                    "browser_screenshot could not foreground Chrome HWND {window_hwnd:#x} before captureVisibleTab; before foreground was {}; focus error: {error}",
                    browser_screenshot_foreground_summary(&before)
                ),
            )
        })?;
    }

    let capture_foreground = read_browser_screenshot_current_foreground("capture_ready")?;
    if capture_foreground.hwnd != window_hwnd {
        tracing::error!(
            code = error_codes::ACTION_POSTCONDITION_FAILED,
            hwnd = window_hwnd,
            before_hwnd = before.hwnd,
            before_pid = before.pid,
            before_process = %before.process_name,
            capture_hwnd = capture_foreground.hwnd,
            capture_pid = capture_foreground.pid,
            capture_process = %capture_foreground.process_name,
            capture_title = %capture_foreground.window_title,
            required_foreground,
            "browser_screenshot foreground precondition failed after explicit activation"
        );
        return Err(mcp_error(
            error_codes::ACTION_POSTCONDITION_FAILED,
            format!(
                "browser_screenshot refused captureVisibleTab because Chrome HWND {window_hwnd:#x} was not the physical OS foreground after activation; actual foreground was {}",
                browser_screenshot_foreground_summary(&capture_foreground)
            ),
        ));
    }

    tracing::info!(
        code = "BROWSER_SCREENSHOT_FOREGROUND_VERIFIED",
        hwnd = window_hwnd,
        human_os_foreground_before_hwnd = before.hwnd,
        human_os_foreground_capture_hwnd = capture_foreground.hwnd,
        human_os_foreground_capture_pid = capture_foreground.pid,
        human_os_foreground_capture_process = %capture_foreground.process_name,
        required_foreground,
        "readback=GetForegroundWindow outcome=target_chrome_foreground_verified"
    );

    Ok(BrowserScreenshotForegroundGuard {
        readback: BrowserScreenshotForegroundReadback {
            required_foreground,
            before_hwnd: Some(before.hwnd),
            capture_hwnd: Some(capture_foreground.hwnd),
            after_restore_hwnd: None,
            restored_human_os_foreground: !required_foreground,
        },
        before,
    })
}

#[cfg(not(windows))]
fn prepare_browser_screenshot_foreground(
    _window_hwnd: i64,
) -> Result<BrowserScreenshotForegroundGuard, ErrorData> {
    Ok(BrowserScreenshotForegroundGuard)
}

#[cfg(windows)]
fn finish_browser_screenshot_foreground(
    window_hwnd: i64,
    guard: BrowserScreenshotForegroundGuard,
    capture_error: Option<&ErrorData>,
) -> Result<BrowserScreenshotForegroundReadback, ErrorData> {
    const TOOL: &str = "browser_screenshot";
    let mut readback = guard.readback;
    let before = guard.before;
    let current = read_browser_screenshot_current_foreground("after_bridge_capture")?;
    readback.after_restore_hwnd = Some(current.hwnd);

    if !readback.required_foreground {
        if current.hwnd == before.hwnd && current.pid == before.pid {
            readback.restored_human_os_foreground = true;
            return Ok(readback);
        }
        tracing::error!(
            code = error_codes::ACTION_POSTCONDITION_FAILED,
            hwnd = window_hwnd,
            before_hwnd = before.hwnd,
            before_pid = before.pid,
            current_hwnd = current.hwnd,
            current_pid = current.pid,
            current_process = %current.process_name,
            current_title = %current.window_title,
            capture_error = ?capture_error,
            "browser_screenshot detected unexpected physical foreground drift while target was already foreground"
        );
        return Err(mcp_error(
            error_codes::ACTION_POSTCONDITION_FAILED,
            format!(
                "browser_screenshot physical foreground drifted from {} to {} during capture",
                browser_screenshot_foreground_summary(&before),
                browser_screenshot_foreground_summary(&current)
            ),
        ));
    }

    if current.hwnd == before.hwnd && current.pid == before.pid {
        readback.restored_human_os_foreground = true;
        tracing::info!(
            code = "BROWSER_SCREENSHOT_FOREGROUND_ALREADY_RESTORED",
            hwnd = window_hwnd,
            human_os_foreground_before_hwnd = before.hwnd,
            human_os_foreground_after_restore_hwnd = current.hwnd,
            capture_error = ?capture_error,
            "readback=GetForegroundWindow outcome=foreground_already_back_at_pre_capture_hwnd"
        );
        return Ok(readback);
    }

    if current.hwnd != window_hwnd {
        tracing::error!(
            code = error_codes::FOREGROUND_RESTORE_SKIPPED_HUMAN_MOVED,
            hwnd = window_hwnd,
            before_hwnd = before.hwnd,
            before_pid = before.pid,
            current_hwnd = current.hwnd,
            current_pid = current.pid,
            current_process = %current.process_name,
            current_title = %current.window_title,
            capture_error = ?capture_error,
            "browser_screenshot refused to restore because physical foreground changed away from the capture HWND"
        );
        return Err(mcp_error(
            error_codes::FOREGROUND_RESTORE_SKIPPED_HUMAN_MOVED,
            format!(
                "browser_screenshot captured with Chrome HWND {window_hwnd:#x}, but physical foreground changed to {} before restore; refusing to overwrite that foreground state",
                browser_screenshot_foreground_summary(&current)
            ),
        ));
    }

    let prior = synapse_a11y::foreground_context(before.hwnd).map_err(|error| {
        mcp_error(
            error_codes::ACTION_FOREGROUND_CONTEXT_RESTORE_FAILED,
            format!(
                "browser_screenshot could not inspect prior foreground HWND {:#x} before restore: {error}",
                before.hwnd
            ),
        )
    })?;
    if prior.pid != before.pid {
        tracing::error!(
            code = error_codes::ACTION_FOREGROUND_CONTEXT_RESTORE_FAILED,
            hwnd = window_hwnd,
            before_hwnd = before.hwnd,
            before_pid = before.pid,
            prior_actual_pid = prior.pid,
            prior_actual_process = %prior.process_name,
            prior_actual_title = %prior.window_title,
            capture_error = ?capture_error,
            "browser_screenshot refused foreground restore because the prior HWND now belongs to another process"
        );
        return Err(mcp_error(
            error_codes::ACTION_FOREGROUND_CONTEXT_RESTORE_FAILED,
            format!(
                "browser_screenshot refused to restore prior foreground HWND {:#x}: expected pid {}, actual pid {}",
                before.hwnd, before.pid, prior.pid
            ),
        ));
    }

    synapse_a11y::focus_window_with_intent(
        before.hwnd,
        synapse_a11y::ForegroundActivationIntent::LeaseContextRestore { caller: TOOL },
    )
    .map_err(|error| {
        mcp_error(
            error_codes::ACTION_FOREGROUND_CONTEXT_RESTORE_FAILED,
            format!(
                "browser_screenshot captured with Chrome HWND {window_hwnd:#x} but failed to restore prior foreground {}; restore error: {error}",
                browser_screenshot_foreground_summary(&before)
            ),
        )
    })?;

    let restored = read_browser_screenshot_current_foreground("after_restore")?;
    readback.after_restore_hwnd = Some(restored.hwnd);
    if restored.hwnd == before.hwnd && restored.pid == before.pid {
        readback.restored_human_os_foreground = true;
        tracing::info!(
            code = "BROWSER_SCREENSHOT_FOREGROUND_RESTORED",
            hwnd = window_hwnd,
            human_os_foreground_before_hwnd = before.hwnd,
            human_os_foreground_before_pid = before.pid,
            human_os_foreground_after_restore_hwnd = restored.hwnd,
            human_os_foreground_after_restore_pid = restored.pid,
            capture_error = ?capture_error,
            "readback=GetForegroundWindow outcome=foreground_restored_to_pre_capture_hwnd"
        );
        Ok(readback)
    } else {
        tracing::error!(
            code = error_codes::ACTION_FOREGROUND_CONTEXT_RESTORE_FAILED,
            hwnd = window_hwnd,
            before_hwnd = before.hwnd,
            before_pid = before.pid,
            restored_hwnd = restored.hwnd,
            restored_pid = restored.pid,
            restored_process = %restored.process_name,
            restored_title = %restored.window_title,
            capture_error = ?capture_error,
            "browser_screenshot foreground restore readback did not match the pre-capture foreground"
        );
        Err(mcp_error(
            error_codes::ACTION_FOREGROUND_CONTEXT_RESTORE_FAILED,
            format!(
                "browser_screenshot restore readback mismatch: expected {}, actual {}",
                browser_screenshot_foreground_summary(&before),
                browser_screenshot_foreground_summary(&restored)
            ),
        ))
    }
}

#[cfg(not(windows))]
fn finish_browser_screenshot_foreground(
    _window_hwnd: i64,
    _guard: BrowserScreenshotForegroundGuard,
    _capture_error: Option<&ErrorData>,
) -> Result<BrowserScreenshotForegroundReadback, ErrorData> {
    Ok(BrowserScreenshotForegroundReadback::default())
}

#[cfg(windows)]
fn read_browser_screenshot_current_foreground(
    phase: &'static str,
) -> Result<ForegroundContext, ErrorData> {
    synapse_a11y::current_foreground_context().map_err(|error| {
        mcp_error(
            error_codes::ACTION_FOREGROUND_CONTEXT_CAPTURE_FAILED,
            format!(
                "browser_screenshot could not read physical OS foreground during {phase}: {error}"
            ),
        )
    })
}

#[cfg(windows)]
fn browser_screenshot_foreground_summary(context: &ForegroundContext) -> String {
    format!(
        "hwnd={:#x} pid={} process={:?} title={:?}",
        context.hwnd, context.pid, context.process_name, context.window_title
    )
}

/// Validates a `set_target` window HWND is live and snapshottable, returning its
/// (title, process_name) so the response confirms exactly which window was bound.
/// Fail-loud: a dead/invalid/unresolvable HWND is `TARGET_WINDOW_NOT_FOUND`.
pub(crate) fn validate_target_window(hwnd: i64) -> Result<(String, String), ErrorData> {
    let context = validate_target_window_context(hwnd)?;
    Ok((context.window_title, context.process_name))
}

fn validate_target_window_context(hwnd: i64) -> Result<ForegroundContext, ErrorData> {
    synapse_capture::validate_hwnd(hwnd).map_err(|error| {
        mcp_error(
            error_codes::TARGET_WINDOW_NOT_FOUND,
            format!("set_target window_hwnd {hwnd:#x} is not a live window: {error}"),
        )
    })?;
    synapse_a11y::foreground_context(hwnd).map_err(|error| {
        mcp_error(
            error_codes::TARGET_WINDOW_NOT_FOUND,
            format!(
                "set_target window_hwnd {hwnd:#x} could not be resolved for perception: {error}"
            ),
        )
    })
}

impl TargetFacadeResponse {
    fn base(operation: TargetOperation) -> Self {
        Self {
            operation,
            source_of_truth: TARGET_FACADE_SOURCE_OF_TRUTH.to_owned(),
            target_state: None,
            windows: None,
            claim: None,
            claim_status: None,
            adopted: None,
            released: None,
        }
    }

    fn target_state(operation: TargetOperation, target_state: TargetResponse) -> Self {
        Self {
            target_state: Some(target_state),
            ..Self::base(operation)
        }
    }

    fn windows(windows: WindowListResponse) -> Self {
        Self {
            windows: Some(windows),
            ..Self::base(TargetOperation::List)
        }
    }

    fn claim(claim: TargetClaimResponse) -> Self {
        Self {
            claim: Some(claim),
            ..Self::base(TargetOperation::Claim)
        }
    }

    fn claim_status(claim_status: TargetClaimStatusResponse) -> Self {
        Self {
            claim_status: Some(claim_status),
            ..Self::base(TargetOperation::Status)
        }
    }

    fn adopted(adopted: TargetClaimAdoptResponse) -> Self {
        Self {
            adopted: Some(adopted),
            ..Self::base(TargetOperation::Adopt)
        }
    }

    fn released(released: TargetReleaseResponse) -> Self {
        Self {
            released: Some(released),
            ..Self::base(TargetOperation::Release)
        }
    }
}

fn target_operation_name(operation: TargetOperation) -> &'static str {
    match operation {
        TargetOperation::Get => "get",
        TargetOperation::List => "list",
        TargetOperation::Set => "set",
        TargetOperation::Clear => "clear",
        TargetOperation::Claim => "claim",
        TargetOperation::Status => "status",
        TargetOperation::Adopt => "adopt",
        TargetOperation::Release => "release",
    }
}

fn target_claim_param_from_set(target: SetTargetParam) -> TargetClaimTargetParam {
    match target {
        SetTargetParam::Window { window_hwnd } => TargetClaimTargetParam::Window { window_hwnd },
        SetTargetParam::Cdp {
            window_hwnd,
            cdp_target_id,
        } => TargetClaimTargetParam::Cdp {
            window_hwnd,
            cdp_target_id,
        },
    }
}

fn validate_target_get_params(params: &TargetParams) -> Result<(), ErrorData> {
    reject_target_facade_fields(
        TargetOperation::Get,
        &[
            ("target", params.target.is_some()),
            ("owner_session_id", params.owner_session_id.is_some()),
            ("ttl_ms", params.ttl_ms.is_some()),
            ("title_contains", params.title_contains.is_some()),
            (
                "process_name_contains",
                params.process_name_contains.is_some(),
            ),
            ("exclude_minimized", params.exclude_minimized),
        ],
    )
}

fn validate_target_list_params(params: &TargetParams) -> Result<(), ErrorData> {
    reject_target_facade_fields(
        TargetOperation::List,
        &[
            ("target", params.target.is_some()),
            ("owner_session_id", params.owner_session_id.is_some()),
            ("ttl_ms", params.ttl_ms.is_some()),
        ],
    )
}

fn validate_target_set_params(params: &TargetParams) -> Result<(), ErrorData> {
    reject_target_facade_fields(
        TargetOperation::Set,
        &[
            ("owner_session_id", params.owner_session_id.is_some()),
            ("ttl_ms", params.ttl_ms.is_some()),
            ("title_contains", params.title_contains.is_some()),
            (
                "process_name_contains",
                params.process_name_contains.is_some(),
            ),
            ("exclude_minimized", params.exclude_minimized),
        ],
    )
}

fn validate_target_clear_params(params: &TargetParams) -> Result<(), ErrorData> {
    reject_target_facade_fields(
        TargetOperation::Clear,
        &[
            ("target", params.target.is_some()),
            ("owner_session_id", params.owner_session_id.is_some()),
            ("ttl_ms", params.ttl_ms.is_some()),
            ("title_contains", params.title_contains.is_some()),
            (
                "process_name_contains",
                params.process_name_contains.is_some(),
            ),
            ("exclude_minimized", params.exclude_minimized),
        ],
    )
}

fn validate_target_claim_params(params: &TargetParams) -> Result<(), ErrorData> {
    reject_target_facade_fields(
        TargetOperation::Claim,
        &[
            ("owner_session_id", params.owner_session_id.is_some()),
            ("title_contains", params.title_contains.is_some()),
            (
                "process_name_contains",
                params.process_name_contains.is_some(),
            ),
            ("exclude_minimized", params.exclude_minimized),
        ],
    )
}

fn validate_target_status_params(params: &TargetParams) -> Result<(), ErrorData> {
    reject_target_facade_fields(
        TargetOperation::Status,
        &[
            ("owner_session_id", params.owner_session_id.is_some()),
            ("ttl_ms", params.ttl_ms.is_some()),
            ("title_contains", params.title_contains.is_some()),
            (
                "process_name_contains",
                params.process_name_contains.is_some(),
            ),
            ("exclude_minimized", params.exclude_minimized),
        ],
    )
}

fn validate_target_adopt_params(params: &TargetParams) -> Result<(), ErrorData> {
    reject_target_facade_fields(
        TargetOperation::Adopt,
        &[
            ("title_contains", params.title_contains.is_some()),
            (
                "process_name_contains",
                params.process_name_contains.is_some(),
            ),
            ("exclude_minimized", params.exclude_minimized),
        ],
    )
}

fn validate_target_release_params(params: &TargetParams) -> Result<(), ErrorData> {
    reject_target_facade_fields(
        TargetOperation::Release,
        &[
            ("owner_session_id", params.owner_session_id.is_some()),
            ("ttl_ms", params.ttl_ms.is_some()),
            ("title_contains", params.title_contains.is_some()),
            (
                "process_name_contains",
                params.process_name_contains.is_some(),
            ),
            ("exclude_minimized", params.exclude_minimized),
        ],
    )
}

fn reject_target_facade_fields(
    operation: TargetOperation,
    fields: &[(&'static str, bool)],
) -> Result<(), ErrorData> {
    if let Some((field, _)) = fields.iter().find(|(_, present)| *present) {
        return Err(target_facade_error(
            operation,
            format!(
                "target operation={} rejects {field}",
                target_operation_name(operation)
            ),
            "remove the operation-irrelevant field or choose the matching target operation",
            field,
        ));
    }
    Ok(())
}

fn target_facade_error(
    operation: TargetOperation,
    message: impl Into<String>,
    remediation: &'static str,
    source_id: &'static str,
) -> ErrorData {
    ErrorData::new(
        ErrorCode(-32099),
        message.into(),
        Some(json!({
            "code": error_codes::TOOL_PARAMS_INVALID,
            "operation": target_operation_name(operation),
            "source_of_truth": TARGET_FACADE_SOURCE_OF_TRUTH,
            "source_id": source_id,
            "remediation": remediation,
        })),
    )
}

fn screenshot_operation_name(operation: ScreenshotOperation) -> &'static str {
    match operation {
        ScreenshotOperation::Capture => "capture",
        ScreenshotOperation::Gif => "gif",
    }
}

fn validate_screenshot_capture_facade_params(params: &ScreenshotParams) -> Result<(), ErrorData> {
    if params.duration_ms.is_some() {
        return Err(screenshot_facade_error(
            ScreenshotOperation::Capture,
            "screenshot operation=capture rejects duration_ms; duration_ms is only valid with operation=gif",
            "remove duration_ms or call screenshot with operation=gif",
            "duration_ms",
        ));
    }
    if params.interval_ms.is_some() {
        return Err(screenshot_facade_error(
            ScreenshotOperation::Capture,
            "screenshot operation=capture rejects interval_ms; interval_ms is only valid with operation=gif",
            "remove interval_ms or call screenshot with operation=gif",
            "interval_ms",
        ));
    }
    Ok(())
}

fn validate_screenshot_gif_facade_params(params: &ScreenshotParams) -> Result<(), ErrorData> {
    if params.region.is_some() {
        return Err(screenshot_facade_error(
            ScreenshotOperation::Gif,
            "screenshot operation=gif rejects region; GIF capture records the whole target window",
            "remove region or call screenshot with operation=capture for a still image crop",
            "region",
        ));
    }
    if params.max_pixels.is_some() {
        return Err(screenshot_facade_error(
            ScreenshotOperation::Gif,
            "screenshot operation=gif rejects max_pixels; GIF capture supports max_long_edge only",
            "remove max_pixels or call screenshot with operation=capture",
            "max_pixels",
        ));
    }
    if !path_has_extension(&params.path, "gif") {
        return Err(screenshot_facade_error(
            ScreenshotOperation::Gif,
            format!(
                "screenshot operation=gif requires an output path ending in .gif: {}",
                params.path
            ),
            "use an absolute .gif output path",
            "path",
        ));
    }
    if let Some(duration_ms) = params.duration_ms
        && !(100..=60_000).contains(&duration_ms)
    {
        return Err(screenshot_facade_error(
            ScreenshotOperation::Gif,
            format!("screenshot operation=gif duration_ms must be 100..=60000; got {duration_ms}"),
            "pass duration_ms between 100 and 60000, or omit it for the default",
            "duration_ms",
        ));
    }
    if let Some(interval_ms) = params.interval_ms
        && interval_ms < 100
    {
        return Err(screenshot_facade_error(
            ScreenshotOperation::Gif,
            format!("screenshot operation=gif interval_ms must be >=100; got {interval_ms}"),
            "pass interval_ms >= 100, or omit it for the default",
            "interval_ms",
        ));
    }
    Ok(())
}

fn path_has_extension(path: &str, expected: &str) -> bool {
    Path::new(path)
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case(expected))
}

fn screenshot_facade_error(
    operation: ScreenshotOperation,
    message: impl Into<String>,
    remediation: &'static str,
    source_id: &'static str,
) -> ErrorData {
    ErrorData::new(
        ErrorCode(-32099),
        message.into(),
        Some(json!({
            "code": error_codes::TOOL_PARAMS_INVALID,
            "operation": screenshot_operation_name(operation),
            "source_of_truth": SCREENSHOT_SOURCE_OF_TRUTH,
            "source_id": source_id,
            "remediation": remediation,
        })),
    )
}

fn hidden_worker_target_miss(error: &ErrorData) -> bool {
    matches!(
        error
            .data
            .as_ref()
            .and_then(|data| data.get("code"))
            .and_then(serde_json::Value::as_str),
        Some(error_codes::TARGET_WINDOW_NOT_FOUND)
    )
}

fn resolve_capture_target_window_context(hwnd: i64) -> Result<ForegroundContext, ErrorData> {
    synapse_capture::validate_hwnd(hwnd).map_err(|error| {
        mcp_error(
            error_codes::TARGET_WINDOW_NOT_FOUND,
            format!("capture_screenshot window_hwnd {hwnd:#x} is not a live window: {error}"),
        )
    })?;
    synapse_a11y::foreground_context(hwnd).map_err(|error| {
        mcp_error(
            error_codes::TARGET_WINDOW_NOT_FOUND,
            format!(
                "capture_screenshot window_hwnd {hwnd:#x} could not be resolved for perception: {error}"
            ),
        )
    })
}

const WINDOW_SCREENSHOT_TIMEOUT_MS: u64 = 1500;
const TRANSIENT_HWND_MAX_EDGE_PX: i32 = 64;

fn capture_screen_screenshot_to_file(
    params: &CaptureScreenshotParams,
    region: Rect,
    foreground: Option<ForegroundContext>,
) -> Result<CaptureScreenshotResponse, ErrorData> {
    validate_screenshot_region(region)?;
    let output_path = screenshot_output_path(&params.path)?;
    let format = screenshot_format_from_path(&output_path)?;
    ensure_screenshot_path_available(&output_path, params.overwrite)?;
    let captured = synapse_capture::screen_region_to_bgra_bitmap(region).map_err(|error| {
        mcp_error(
            error.code(),
            format!("capture_screenshot failed for region {region:?}: {error}"),
        )
    })?;
    let bitmap_sha256 = sha256_hex(&captured.bytes);
    write_screenshot_bitmap(
        params,
        output_path,
        format,
        captured,
        "gdi_screen_region_bgra",
        bitmap_sha256,
        foreground,
        None,
    )
}

fn capture_target_window_screenshot_to_file(
    params: &CaptureScreenshotParams,
    window_hwnd: i64,
    region: Rect,
    foreground: Option<ForegroundContext>,
) -> Result<CaptureScreenshotResponse, ErrorData> {
    validate_screenshot_region(region)?;
    let output_path = screenshot_output_path(&params.path)?;
    let format = screenshot_format_from_path(&output_path)?;
    ensure_screenshot_path_available(&output_path, params.overwrite)?;
    let captured = synapse_capture::window_region_to_bgra_bitmap(
        window_hwnd,
        region,
        WINDOW_SCREENSHOT_TIMEOUT_MS,
    )
    .map_err(|error| {
        capture_target_window_screenshot_error(window_hwnd, region, foreground.as_ref(), error)
    })?;
    let capture_backend = captured.capture_backend;
    let capture_retry_evidence =
        (captured.capture_retry_count > 0).then_some(CaptureRetryEvidence {
            attempts: captured.capture_attempts,
            retry_count: captured.capture_retry_count,
            elapsed_ms: captured.capture_elapsed_ms,
            retry_backoff_ms: captured.capture_retry_backoff_ms,
        });
    let captured = captured.bitmap;
    let bitmap_sha256 = sha256_hex(&captured.bytes);
    write_screenshot_bitmap(
        params,
        output_path,
        format,
        captured,
        capture_backend,
        bitmap_sha256,
        foreground,
        capture_retry_evidence,
    )
}

#[derive(Clone, Debug)]
struct CaptureTargetWindowCandidateDiagnostic {
    message: String,
    candidates: Vec<Value>,
}

fn capture_target_window_screenshot_error(
    window_hwnd: i64,
    region: Rect,
    foreground: Option<&ForegroundContext>,
    error: synapse_capture::CaptureError,
) -> ErrorData {
    let target_detail = foreground
        .map(|context| {
            format!(
                "pid={} process={} title={:?} bounds={:?}",
                context.pid, context.process_name, context.window_title, context.window_bounds
            )
        })
        .unwrap_or_else(|| "pid/title/process/bounds unavailable".to_owned());
    let candidate_diagnostic = capture_target_window_transient_candidate_diagnostic(
        window_hwnd,
        foreground,
        "capture_screenshot",
    );
    let candidate_message = candidate_diagnostic
        .as_ref()
        .map(|diagnostic| format!("; transient_hwnd_diagnostic={}", diagnostic.message))
        .unwrap_or_default();
    ErrorData::new(
        ErrorCode(-32099),
        format!(
            "capture_screenshot failed for target window {window_hwnd:#x} ({target_detail}) region {region:?}: {error}; recommended_next_action=retry capture_screenshot after confirming the target HWND/PID/title is still live and visually stable{candidate_message}"
        ),
        Some(json!({
            "code": error.code(),
            "target_window_hwnd": window_hwnd,
            "target_region": region,
            "target_pid": foreground.map(|context| context.pid),
            "target_process": foreground.map(|context| context.process_name.as_str()),
            "target_title": foreground.map(|context| context.window_title.as_str()),
            "target_window_bounds": foreground.map(|context| context.window_bounds),
            "probable_transient_startup_hwnd": foreground
                .map(capture_context_looks_like_transient_startup_hwnd)
                .unwrap_or(false),
            "same_pid_visible_window_candidates": candidate_diagnostic
                .as_ref()
                .map(|diagnostic| diagnostic.candidates.clone())
                .unwrap_or_default(),
            "recommended_next_action": "retry capture_screenshot after confirming the target HWND/PID/title is still live and visually stable; if same_pid_visible_window_candidates is non-empty, rebind to the named candidate HWND explicitly before retrying",
        })),
    )
}

fn capture_target_window_transient_candidate_diagnostic(
    window_hwnd: i64,
    target: Option<&ForegroundContext>,
    caller: &'static str,
) -> Option<CaptureTargetWindowCandidateDiagnostic> {
    let target = target?;
    if !capture_context_looks_like_transient_startup_hwnd(target) {
        return None;
    }
    let contexts = match synapse_a11y::visible_top_level_window_contexts() {
        Ok(contexts) => contexts,
        Err(error) => {
            tracing::warn!(
                code = "CAPTURE_TARGET_WINDOW_CANDIDATE_ENUMERATION_FAILED",
                hwnd = window_hwnd,
                pid = target.pid,
                caller,
                error = %error,
                "could not enumerate same-PID visible window candidates after target capture failure"
            );
            return None;
        }
    };
    capture_target_window_transient_candidate_diagnostic_from_contexts(
        window_hwnd,
        target,
        contexts,
    )
}

fn capture_target_window_transient_candidate_diagnostic_from_contexts(
    window_hwnd: i64,
    target: &ForegroundContext,
    contexts: impl IntoIterator<Item = ForegroundContext>,
) -> Option<CaptureTargetWindowCandidateDiagnostic> {
    if !capture_context_looks_like_transient_startup_hwnd(target) {
        return None;
    }
    let candidates: Vec<_> = contexts
        .into_iter()
        .filter(|context| context.pid == target.pid)
        .filter(|context| context.hwnd != window_hwnd)
        .filter(|context| !context.window_title.trim().is_empty())
        .filter(|context| {
            context.window_bounds.w > target.window_bounds.w
                || context.window_bounds.h > target.window_bounds.h
        })
        .take(5)
        .collect();
    if candidates.is_empty() {
        return None;
    }
    let candidate_summary = candidates
        .iter()
        .map(|context| {
            format!(
                "hwnd={:#x} pid={} process={} title={:?} bounds={:?}",
                context.hwnd,
                context.pid,
                context.process_name,
                context.window_title,
                context.window_bounds
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    Some(CaptureTargetWindowCandidateDiagnostic {
        message: format!(
            "target HWND looks like a transient startup/stub window; same-PID visible candidates: {candidate_summary}"
        ),
        candidates: candidates
            .iter()
            .map(|context| {
                json!({
                    "hwnd": context.hwnd,
                    "pid": context.pid,
                    "process_name": context.process_name,
                    "window_title": context.window_title,
                    "window_bounds": context.window_bounds,
                })
            })
            .collect(),
    })
}

fn capture_context_looks_like_transient_startup_hwnd(context: &ForegroundContext) -> bool {
    context.window_title.trim().is_empty()
        || context.window_bounds.w <= TRANSIENT_HWND_MAX_EDGE_PX
        || context.window_bounds.h <= TRANSIENT_HWND_MAX_EDGE_PX
}

/// #1341/#1343/#1517: true when a browser_screenshot bridge capture failed
/// because the normal Chrome bridge direct-HTTP host disconnected mid-command.
/// The only accepted recovery is a primary-lane retry after the bridge host
/// reconnects; passive HWND capture is not an equivalent substitute.
fn browser_screenshot_bridge_disconnected(error: &ErrorData) -> bool {
    error
        .message
        .contains("disconnected before command response")
        || error
            .message
            .contains("client closed direct HTTP WebSocket")
}

async fn browser_screenshot_retry_after_bridge_disconnect(
    window_hwnd: i64,
    cdp_target_id: &str,
    bridge_payload: Value,
    first_error: &ErrorData,
) -> Result<crate::chrome_debugger_bridge::ChromeDebuggerPageScreenshotResult, ErrorData> {
    tracing::warn!(
        code = "BROWSER_SCREENSHOT_BRIDGE_RECONNECT_RETRY",
        hwnd = window_hwnd,
        cdp_target_id = %cdp_target_id,
        wait_timeout_ms = BROWSER_SCREENSHOT_BRIDGE_RECONNECT_RETRY_WAIT_MS,
        first_error = %first_error.message,
        "browser_screenshot primary Chrome bridge disconnected mid-capture; waiting for bridge reconnect before one primary-lane retry"
    );
    let reconnect = crate::chrome_debugger_bridge::wait_for_active_bridge_host(
        BROWSER_SCREENSHOT_BRIDGE_RECONNECT_RETRY_WAIT_MS,
    )
    .await
    .map_err(|error| {
        mcp_error(
            error.code(),
            format!(
                "browser_screenshot Chrome bridge disconnected during capture and did not expose a usable reconnected host within {BROWSER_SCREENSHOT_BRIDGE_RECONNECT_RETRY_WAIT_MS} ms; first_error={}; reconnect_error={}. remediation=check health chrome_bridge last_disconnect_detail, reload the installed Synapse Chrome Bridge, then retry the same target-scoped browser_capture. No passive WGC fallback was used.",
                first_error.message,
                error.detail()
            ),
        )
    })?;
    tracing::info!(
        code = "BROWSER_SCREENSHOT_BRIDGE_RECONNECTED_FOR_RETRY",
        hwnd = window_hwnd,
        cdp_target_id = %cdp_target_id,
        host_id = %reconnect.host_id,
        registered_unix_ms = reconnect.registered_unix_ms,
        last_seen_unix_ms = reconnect.last_seen_unix_ms,
        "readback=chrome_bridge_active_host_snapshot outcome=reconnected_before_screenshot_retry"
    );
    crate::chrome_debugger_bridge::page_screenshot(window_hwnd, cdp_target_id, bridge_payload)
        .await
        .map_err(|retry_error| {
            let retry_detail = retry_error.detail().to_owned();
            let retry_code = retry_error.code().to_owned();
            let code = retry_error.code();
            mcp_error(
                code,
                format!(
                    "browser_screenshot Chrome bridge capture failed after reconnect retry; first_error={}; retry_code={retry_code}; retry_error={retry_detail}; host_id={}. No passive WGC fallback was used; the screenshot artifact was not written from a substitute capture lane.",
                    first_error.message,
                    reconnect.host_id
                ),
            )
        })
}

#[cfg(windows)]
fn chrome_capture_visible_tab_data_url_to_bgra(
    data_url: &str,
    region: Option<Rect>,
) -> Result<synapse_capture::CapturedBgraBitmap, ErrorData> {
    let (header, encoded) = data_url.split_once(',').ok_or_else(|| {
        mcp_error(
            error_codes::A11Y_CDP_AXTREE_FAILED,
            "capture_screenshot Chrome bridge returned malformed image data URL",
        )
    })?;
    let header_lower = header.to_ascii_lowercase();
    if !header_lower.starts_with("data:image/") || !header_lower.contains(";base64") {
        return Err(mcp_error(
            error_codes::A11Y_CDP_AXTREE_FAILED,
            format!(
                "capture_screenshot Chrome bridge returned unsupported image data URL header {header:?}"
            ),
        ));
    }
    let image_bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded.trim())
        .map_err(|error| {
            mcp_error(
                error_codes::A11Y_CDP_AXTREE_FAILED,
                format!("capture_screenshot could not decode bridge screenshot base64: {error}"),
            )
        })?;
    let rgba = image::load_from_memory(&image_bytes)
        .map_err(|error| {
            mcp_error(
                error_codes::A11Y_CDP_AXTREE_FAILED,
                format!("capture_screenshot could not decode bridge screenshot image: {error}"),
            )
        })?
        .to_rgba8();
    let width = rgba.width();
    let height = rgba.height();
    let mut bgra = rgba.into_raw();
    for pixel in bgra.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }
    let bitmap = synapse_capture::CapturedBgraBitmap {
        region: bitmap_full_region(width, height)?,
        width,
        height,
        bytes: bgra,
    };
    match region {
        Some(region) => crop_bgra_bitmap(bitmap, region),
        None => Ok(bitmap),
    }
}

#[cfg(windows)]
fn cdp_page_bitmap_to_captured_bgra(
    page_bitmap: synapse_a11y::CdpNodeBitmap,
    region: Option<Rect>,
) -> Result<synapse_capture::CapturedBgraBitmap, ErrorData> {
    Ok(synapse_capture::CapturedBgraBitmap {
        region: region.unwrap_or(bitmap_full_region(page_bitmap.width, page_bitmap.height)?),
        width: page_bitmap.width,
        height: page_bitmap.height,
        bytes: page_bitmap.bgra,
    })
}

struct BrowserScreenshotValidation {
    output_path: PathBuf,
    format: CaptureScreenshotFormat,
    element_target: Option<String>,
    mask_target: Option<String>,
}

fn validate_browser_screenshot_params(
    params: &BrowserScreenshotParams,
) -> Result<BrowserScreenshotValidation, ErrorData> {
    let output_path = screenshot_output_path(&params.path)?;
    let path_format = screenshot_format_from_path(&output_path)?;
    let format = params.format.unwrap_or(path_format);
    if format != path_format {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "browser_screenshot format {format:?} does not match output extension for {}",
                output_path.display()
            ),
        ));
    }
    if let Some(quality) = params.quality
        && quality > 100
    {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("browser_screenshot quality must be 0..=100; got {quality}"),
        ));
    }
    if params.max_pixels == Some(0) {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "browser_screenshot max_pixels must be greater than zero",
        ));
    }
    if params.max_long_edge == Some(0) {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "browser_screenshot max_long_edge must be greater than zero",
        ));
    }
    match params.scope {
        BrowserScreenshotScope::Viewport | BrowserScreenshotScope::FullPage => {
            if params.clip.is_some() || params.element_id.is_some() {
                return Err(mcp_error(
                    error_codes::TOOL_PARAMS_INVALID,
                    "browser_screenshot scope=viewport/full_page rejects clip and element_id",
                ));
            }
        }
        BrowserScreenshotScope::Clip => {
            let Some(clip) = params.clip else {
                return Err(mcp_error(
                    error_codes::TOOL_PARAMS_INVALID,
                    "browser_screenshot scope=clip requires clip",
                ));
            };
            if params.element_id.is_some() {
                return Err(mcp_error(
                    error_codes::TOOL_PARAMS_INVALID,
                    "browser_screenshot scope=clip rejects element_id",
                ));
            }
            validate_browser_screenshot_clip(clip)?;
        }
        BrowserScreenshotScope::Element => {
            if params.clip.is_some() {
                return Err(mcp_error(
                    error_codes::TOOL_PARAMS_INVALID,
                    "browser_screenshot scope=element rejects clip",
                ));
            }
            if params.element_id.as_deref().is_none_or(str::is_empty) {
                return Err(mcp_error(
                    error_codes::TOOL_PARAMS_INVALID,
                    "browser_screenshot scope=element requires element_id",
                ));
            }
        }
    }
    let element_target = params
        .element_id
        .as_deref()
        .map(parse_chrome_bridge_element_target)
        .transpose()?
        .flatten();
    let mut mask_target: Option<String> = None;
    for (index, mask) in params.masks.iter().enumerate() {
        let has_selector = mask
            .selector
            .as_deref()
            .is_some_and(|value| !value.is_empty());
        let has_element = mask
            .element_id
            .as_deref()
            .is_some_and(|value| !value.is_empty());
        if has_selector == has_element {
            return Err(mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                format!(
                    "browser_screenshot masks[{index}] requires exactly one of selector or element_id"
                ),
            ));
        }
        if let Some(selector) = mask.selector.as_deref()
            && selector.chars().count() > 4096
        {
            return Err(mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                format!("browser_screenshot masks[{index}].selector exceeds 4096 characters"),
            ));
        }
        if let Some(color) = mask.color.as_deref()
            && (color.is_empty() || color.chars().count() > 128 || color.contains('\0'))
        {
            return Err(mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                format!("browser_screenshot masks[{index}].color is invalid"),
            ));
        }
        if let Some(element_id) = mask.element_id.as_deref() {
            let target = parse_chrome_bridge_element_target(element_id)?.ok_or_else(|| {
                mcp_error(
                    error_codes::TOOL_PARAMS_INVALID,
                    format!(
                        "browser_screenshot masks[{index}].element_id must be a normal Chrome bridge element id"
                    ),
                )
            })?;
            if let Some(existing) = mask_target.as_ref() {
                if !cdp_target_ids_equal(existing, &target) {
                    return Err(mcp_error(
                        error_codes::TOOL_PARAMS_INVALID,
                        format!(
                            "browser_screenshot mask element ids span multiple targets: {existing:?} and {target:?}"
                        ),
                    ));
                }
            } else {
                mask_target = Some(target);
            }
        }
    }
    Ok(BrowserScreenshotValidation {
        output_path,
        format,
        element_target,
        mask_target,
    })
}

struct BrowserPdfValidation {
    output_path: PathBuf,
}

fn validate_browser_pdf_params(
    params: &BrowserPdfParams,
) -> Result<BrowserPdfValidation, ErrorData> {
    let output_path = browser_pdf_output_path(&params.path)?;
    validate_browser_pdf_number(params.scale, "scale", 0.1, 2.0)?;
    validate_browser_pdf_number(params.paper_width, "paper_width", 0.1, 200.0)?;
    validate_browser_pdf_number(params.paper_height, "paper_height", 0.1, 200.0)?;
    validate_browser_pdf_number(params.margin_top, "margin_top", 0.0, 20.0)?;
    validate_browser_pdf_number(params.margin_bottom, "margin_bottom", 0.0, 20.0)?;
    validate_browser_pdf_number(params.margin_left, "margin_left", 0.0, 20.0)?;
    validate_browser_pdf_number(params.margin_right, "margin_right", 0.0, 20.0)?;
    if let Some(page_ranges) = params.page_ranges.as_deref()
        && page_ranges.len() > 1024
    {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "browser_pdf page_ranges exceeds 1024 bytes",
        ));
    }
    if let Some(header_template) = params.header_template.as_deref()
        && header_template.len() > 8192
    {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "browser_pdf header_template exceeds 8192 bytes",
        ));
    }
    if let Some(footer_template) = params.footer_template.as_deref()
        && footer_template.len() > 8192
    {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "browser_pdf footer_template exceeds 8192 bytes",
        ));
    }
    if let Some(wait_timeout_ms) = params.wait_timeout_ms
        && wait_timeout_ms > 60_000
    {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("browser_pdf wait_timeout_ms must be <= 60000; got {wait_timeout_ms}"),
        ));
    }
    Ok(BrowserPdfValidation { output_path })
}

fn validate_browser_pdf_number(
    value: Option<f64>,
    name: &str,
    min: f64,
    max: f64,
) -> Result<(), ErrorData> {
    if let Some(value) = value
        && (!value.is_finite() || value < min || value > max)
    {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("browser_pdf {name} must be finite and in {min}..={max}; got {value}"),
        ));
    }
    Ok(())
}

fn browser_pdf_bridge_payload(params: &BrowserPdfParams) -> Value {
    json!({
        "landscape": params.landscape,
        "printBackground": params.print_background,
        "displayHeaderFooter": params.display_header_footer,
        "headerTemplate": params.header_template.as_deref(),
        "footerTemplate": params.footer_template.as_deref(),
        "scale": params.scale,
        "paperWidth": params.paper_width,
        "paperHeight": params.paper_height,
        "marginTop": params.margin_top,
        "marginBottom": params.margin_bottom,
        "marginLeft": params.margin_left,
        "marginRight": params.margin_right,
        "pageRanges": params.page_ranges.as_deref(),
        "preferCSSPageSize": params.prefer_css_page_size,
        "waitTimeoutMs": params.wait_timeout_ms,
    })
}

fn validate_browser_screenshot_clip(clip: Rect) -> Result<(), ErrorData> {
    if clip.x < 0 || clip.y < 0 || clip.w <= 0 || clip.h <= 0 {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "browser_screenshot clip must be page-CSS non-negative and non-empty: bbox=({}, {}, {}, {})",
                clip.x, clip.y, clip.w, clip.h
            ),
        ));
    }
    Ok(())
}

fn validate_browser_screenshot_target_ids(
    validation: &BrowserScreenshotValidation,
    cdp_target_id: &str,
) -> Result<(), ErrorData> {
    for (label, target) in [
        ("element_id", validation.element_target.as_deref()),
        ("mask element_id", validation.mask_target.as_deref()),
    ] {
        if let Some(target) = target
            && !cdp_target_ids_equal(target, cdp_target_id)
        {
            return Err(mcp_error(
                error_codes::ACTION_TARGET_INVALID,
                format!(
                    "browser_screenshot {label} resolves to target {target:?}, but capture target is {cdp_target_id:?}"
                ),
            ));
        }
    }
    Ok(())
}

fn browser_screenshot_bridge_payload(
    params: &BrowserScreenshotParams,
    format: CaptureScreenshotFormat,
) -> Result<Value, ErrorData> {
    let masks = params
        .masks
        .iter()
        .map(|mask| {
            json!({
                "selector": mask.selector.as_deref(),
                "elementId": mask.element_id.as_deref(),
                "color": mask.color.as_deref(),
            })
        })
        .collect::<Vec<_>>();
    Ok(json!({
        "scope": browser_screenshot_scope_str(params.scope),
        "clip": params.clip.map(|clip| json!({
            "x": clip.x,
            "y": clip.y,
            "w": clip.w,
            "h": clip.h,
        })),
        "elementId": params.element_id.as_deref(),
        "masks": masks,
        "format": match format {
            CaptureScreenshotFormat::Png => "png",
            CaptureScreenshotFormat::Jpeg => "jpeg",
        },
        "quality": params.quality.unwrap_or(90),
        "omitBackground": params.omit_background,
        "waitTimeoutMs": params.wait_timeout_ms,
    }))
}

fn browser_screenshot_scope_str(scope: BrowserScreenshotScope) -> &'static str {
    match scope {
        BrowserScreenshotScope::Viewport => "viewport",
        BrowserScreenshotScope::FullPage => "full_page",
        BrowserScreenshotScope::Clip => "clip",
        BrowserScreenshotScope::Element => "element",
    }
}

fn browser_screenshot_page_region(
    clip: crate::chrome_debugger_bridge::ChromeDebuggerPageScreenshotRect,
) -> Result<Rect, ErrorData> {
    let x = f64_to_i32_rounded(clip.x, "browser_screenshot clip.x")?;
    let y = f64_to_i32_rounded(clip.y, "browser_screenshot clip.y")?;
    let w = f64_to_i32_rounded(clip.w, "browser_screenshot clip.w")?;
    let h = f64_to_i32_rounded(clip.h, "browser_screenshot clip.h")?;
    validate_browser_screenshot_clip(Rect { x, y, w, h })?;
    Ok(Rect { x, y, w, h })
}

fn f64_to_i32_rounded(value: f64, label: &str) -> Result<i32, ErrorData> {
    if !value.is_finite() || value < f64::from(i32::MIN) || value > f64::from(i32::MAX) {
        return Err(mcp_error(
            error_codes::CAPTURE_TARGET_INVALID,
            format!("{label} is not a finite i32-compatible value: {value}"),
        ));
    }
    Ok(value.round() as i32)
}

fn stitch_browser_screenshot_tiles(
    captured: &crate::chrome_debugger_bridge::ChromeDebuggerPageScreenshotResult,
    format: CaptureScreenshotFormat,
    omit_background: bool,
) -> Result<synapse_capture::CapturedBgraBitmap, ErrorData> {
    if captured.tiles.is_empty() {
        return Err(mcp_error(
            error_codes::A11Y_CDP_AXTREE_FAILED,
            "browser_screenshot Chrome bridge returned no screenshot tiles",
        ));
    }
    let first = &captured.tiles[0];
    let first_image = browser_screenshot_data_url_to_rgba(&first.image_data_url)?;
    let scale_x = browser_screenshot_tile_scale(
        first_image.width(),
        first.viewport_width_css,
        "viewport_width_css",
    )?;
    let scale_y = browser_screenshot_tile_scale(
        first_image.height(),
        first.viewport_height_css,
        "viewport_height_css",
    )?;
    let output_width = f64_to_u32_ceil(captured.clip_css.w * scale_x, "output width")?;
    let output_height = f64_to_u32_ceil(captured.clip_css.h * scale_y, "output height")?;
    let mut output = RgbaImage::new(output_width, output_height);
    blit_browser_screenshot_tile(
        &mut output,
        &first_image,
        first,
        captured.clip_css,
        scale_x,
        scale_y,
    )?;
    for tile in captured.tiles.iter().skip(1) {
        let image = browser_screenshot_data_url_to_rgba(&tile.image_data_url)?;
        blit_browser_screenshot_tile(
            &mut output,
            &image,
            tile,
            captured.clip_css,
            scale_x,
            scale_y,
        )?;
    }
    if omit_background && matches!(format, CaptureScreenshotFormat::Png) {
        browser_screenshot_omit_background_by_corner(&mut output);
    }
    let mut bgra = output.into_raw();
    for pixel in bgra.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }
    Ok(synapse_capture::CapturedBgraBitmap {
        region: Rect {
            x: 0,
            y: 0,
            w: i32::try_from(output_width).map_err(|_| {
                mcp_error(
                    error_codes::CAPTURE_TARGET_INVALID,
                    format!("browser_screenshot output width {output_width} exceeds i32"),
                )
            })?,
            h: i32::try_from(output_height).map_err(|_| {
                mcp_error(
                    error_codes::CAPTURE_TARGET_INVALID,
                    format!("browser_screenshot output height {output_height} exceeds i32"),
                )
            })?,
        },
        width: output_width,
        height: output_height,
        bytes: bgra,
    })
}

fn browser_screenshot_tile_scale(
    image_extent: u32,
    viewport_extent_css: f64,
    label: &str,
) -> Result<f64, ErrorData> {
    if image_extent == 0 || !viewport_extent_css.is_finite() || viewport_extent_css <= 0.0 {
        return Err(mcp_error(
            error_codes::A11Y_CDP_AXTREE_FAILED,
            format!(
                "browser_screenshot tile has invalid {label}: image_extent={image_extent} viewport_extent_css={viewport_extent_css}"
            ),
        ));
    }
    Ok(f64::from(image_extent) / viewport_extent_css)
}

fn f64_to_u32_ceil(value: f64, label: &str) -> Result<u32, ErrorData> {
    if !value.is_finite() || value <= 0.0 || value > f64::from(u32::MAX) {
        return Err(mcp_error(
            error_codes::CAPTURE_TARGET_INVALID,
            format!("browser_screenshot {label} is invalid: {value}"),
        ));
    }
    Ok(value.ceil() as u32)
}

fn browser_screenshot_data_url_to_rgba(data_url: &str) -> Result<RgbaImage, ErrorData> {
    let (header, encoded) = data_url.split_once(',').ok_or_else(|| {
        mcp_error(
            error_codes::A11Y_CDP_AXTREE_FAILED,
            "browser_screenshot Chrome bridge returned malformed image data URL",
        )
    })?;
    let header_lower = header.to_ascii_lowercase();
    if !header_lower.starts_with("data:image/") || !header_lower.contains(";base64") {
        return Err(mcp_error(
            error_codes::A11Y_CDP_AXTREE_FAILED,
            format!(
                "browser_screenshot Chrome bridge returned unsupported image data URL header {header:?}"
            ),
        ));
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded.trim())
        .map_err(|error| {
            mcp_error(
                error_codes::A11Y_CDP_AXTREE_FAILED,
                format!("browser_screenshot could not decode tile base64: {error}"),
            )
        })?;
    Ok(image::load_from_memory(&bytes)
        .map_err(|error| {
            mcp_error(
                error_codes::A11Y_CDP_AXTREE_FAILED,
                format!("browser_screenshot could not decode tile image: {error}"),
            )
        })?
        .to_rgba8())
}

fn blit_browser_screenshot_tile(
    output: &mut RgbaImage,
    tile_image: &RgbaImage,
    tile: &crate::chrome_debugger_bridge::ChromeDebuggerPageScreenshotTile,
    clip: crate::chrome_debugger_bridge::ChromeDebuggerPageScreenshotRect,
    output_scale_x: f64,
    output_scale_y: f64,
) -> Result<(), ErrorData> {
    let tile_scale_x = browser_screenshot_tile_scale(
        tile_image.width(),
        tile.viewport_width_css,
        "tile viewport_width_css",
    )?;
    let tile_scale_y = browser_screenshot_tile_scale(
        tile_image.height(),
        tile.viewport_height_css,
        "tile viewport_height_css",
    )?;
    let left = clip.x.max(tile.scroll_x_css);
    let top = clip.y.max(tile.scroll_y_css);
    let right = (clip.x + clip.w).min(tile.scroll_x_css + tile.viewport_width_css);
    let bottom = (clip.y + clip.h).min(tile.scroll_y_css + tile.viewport_height_css);
    if right <= left || bottom <= top {
        return Ok(());
    }
    let dest_x0 = ((left - clip.x) * output_scale_x).floor().max(0.0) as u32;
    let dest_y0 = ((top - clip.y) * output_scale_y).floor().max(0.0) as u32;
    let dest_x1 = ((right - clip.x) * output_scale_x)
        .ceil()
        .min(f64::from(output.width())) as u32;
    let dest_y1 = ((bottom - clip.y) * output_scale_y)
        .ceil()
        .min(f64::from(output.height())) as u32;
    for dest_y in dest_y0..dest_y1 {
        let css_y = clip.y + (f64::from(dest_y) + 0.5) / output_scale_y;
        let source_y = ((css_y - tile.scroll_y_css) * tile_scale_y)
            .floor()
            .clamp(0.0, f64::from(tile_image.height().saturating_sub(1)))
            as u32;
        for dest_x in dest_x0..dest_x1 {
            let css_x = clip.x + (f64::from(dest_x) + 0.5) / output_scale_x;
            let source_x = ((css_x - tile.scroll_x_css) * tile_scale_x)
                .floor()
                .clamp(0.0, f64::from(tile_image.width().saturating_sub(1)))
                as u32;
            let pixel = *tile_image.get_pixel(source_x, source_y);
            output.put_pixel(dest_x, dest_y, pixel);
        }
    }
    Ok(())
}

fn browser_screenshot_omit_background_by_corner(image: &mut RgbaImage) {
    if image.width() == 0 || image.height() == 0 {
        return;
    }
    let bg = *image.get_pixel(0, 0);
    if bg[3] < 255 {
        return;
    }
    for pixel in image.pixels_mut() {
        let close = pixel[0].abs_diff(bg[0]) <= 2
            && pixel[1].abs_diff(bg[1]) <= 2
            && pixel[2].abs_diff(bg[2]) <= 2;
        if close {
            pixel[3] = 0;
        }
    }
}

fn bitmap_full_region(width: u32, height: u32) -> Result<Rect, ErrorData> {
    Ok(Rect {
        x: 0,
        y: 0,
        w: i32::try_from(width).map_err(|_| {
            mcp_error(
                error_codes::CAPTURE_TARGET_INVALID,
                format!("capture_screenshot bitmap width {width} exceeds i32"),
            )
        })?,
        h: i32::try_from(height).map_err(|_| {
            mcp_error(
                error_codes::CAPTURE_TARGET_INVALID,
                format!("capture_screenshot bitmap height {height} exceeds i32"),
            )
        })?,
    })
}

fn crop_bgra_bitmap(
    bitmap: synapse_capture::CapturedBgraBitmap,
    region: Rect,
) -> Result<synapse_capture::CapturedBgraBitmap, ErrorData> {
    validate_screenshot_region(region)?;
    if region.x < 0 || region.y < 0 {
        return Err(mcp_error(
            error_codes::CAPTURE_TARGET_INVALID,
            format!(
                "capture_screenshot region for browser target must be viewport-relative and non-negative: bbox=({}, {}, {}, {})",
                region.x, region.y, region.w, region.h
            ),
        ));
    }
    let x = usize::try_from(region.x).map_err(|_| {
        mcp_error(
            error_codes::CAPTURE_TARGET_INVALID,
            format!("capture_screenshot region x {} is invalid", region.x),
        )
    })?;
    let y = usize::try_from(region.y).map_err(|_| {
        mcp_error(
            error_codes::CAPTURE_TARGET_INVALID,
            format!("capture_screenshot region y {} is invalid", region.y),
        )
    })?;
    let w = usize::try_from(region.w).map_err(|_| {
        mcp_error(
            error_codes::CAPTURE_TARGET_INVALID,
            format!("capture_screenshot region width {} is invalid", region.w),
        )
    })?;
    let h = usize::try_from(region.h).map_err(|_| {
        mcp_error(
            error_codes::CAPTURE_TARGET_INVALID,
            format!("capture_screenshot region height {} is invalid", region.h),
        )
    })?;
    let bitmap_width = usize::try_from(bitmap.width).map_err(|_| {
        mcp_error(
            error_codes::CAPTURE_TARGET_INVALID,
            format!(
                "capture_screenshot bitmap width {} exceeds usize",
                bitmap.width
            ),
        )
    })?;
    let bitmap_height = usize::try_from(bitmap.height).map_err(|_| {
        mcp_error(
            error_codes::CAPTURE_TARGET_INVALID,
            format!(
                "capture_screenshot bitmap height {} exceeds usize",
                bitmap.height
            ),
        )
    })?;
    if x.checked_add(w).is_none_or(|right| right > bitmap_width)
        || y.checked_add(h).is_none_or(|bottom| bottom > bitmap_height)
    {
        return Err(mcp_error(
            error_codes::CAPTURE_TARGET_INVALID,
            format!(
                "capture_screenshot browser target region bbox=({}, {}, {}, {}) exceeds captured bitmap {}x{}",
                region.x, region.y, region.w, region.h, bitmap.width, bitmap.height
            ),
        ));
    }
    let row_bytes = bitmap_width.checked_mul(4).ok_or_else(|| {
        mcp_error(
            error_codes::CAPTURE_TARGET_INVALID,
            format!(
                "capture_screenshot bitmap row width {} overflows",
                bitmap.width
            ),
        )
    })?;
    let crop_row_bytes = w.checked_mul(4).ok_or_else(|| {
        mcp_error(
            error_codes::CAPTURE_TARGET_INVALID,
            format!("capture_screenshot crop row width {} overflows", region.w),
        )
    })?;
    let capacity = crop_row_bytes.checked_mul(h).ok_or_else(|| {
        mcp_error(
            error_codes::CAPTURE_TARGET_INVALID,
            "capture_screenshot crop byte length overflows",
        )
    })?;
    let mut cropped = Vec::with_capacity(capacity);
    for row in y..(y + h) {
        let start = row
            .checked_mul(row_bytes)
            .and_then(|offset| offset.checked_add(x * 4))
            .ok_or_else(|| {
                mcp_error(
                    error_codes::CAPTURE_TARGET_INVALID,
                    "capture_screenshot crop offset overflows",
                )
            })?;
        let end = start.checked_add(crop_row_bytes).ok_or_else(|| {
            mcp_error(
                error_codes::CAPTURE_TARGET_INVALID,
                "capture_screenshot crop end offset overflows",
            )
        })?;
        let slice = bitmap.bytes.get(start..end).ok_or_else(|| {
            mcp_error(
                error_codes::CAPTURE_TARGET_INVALID,
                "capture_screenshot crop range exceeds bitmap byte buffer",
            )
        })?;
        cropped.extend_from_slice(slice);
    }
    Ok(synapse_capture::CapturedBgraBitmap {
        region,
        width: u32::try_from(w).map_err(|_| {
            mcp_error(
                error_codes::CAPTURE_TARGET_INVALID,
                format!("capture_screenshot crop width {w} exceeds u32"),
            )
        })?,
        height: u32::try_from(h).map_err(|_| {
            mcp_error(
                error_codes::CAPTURE_TARGET_INVALID,
                format!("capture_screenshot crop height {h} exceeds u32"),
            )
        })?,
        bytes: cropped,
    })
}

fn write_screenshot_bitmap(
    params: &CaptureScreenshotParams,
    output_path: PathBuf,
    format: CaptureScreenshotFormat,
    captured: synapse_capture::CapturedBgraBitmap,
    capture_backend: &str,
    bitmap_sha256: String,
    foreground: Option<ForegroundContext>,
    capture_retry_evidence: Option<CaptureRetryEvidence>,
) -> Result<CaptureScreenshotResponse, ErrorData> {
    write_screenshot_bitmap_with_quality(
        params,
        output_path,
        format,
        captured,
        capture_backend,
        bitmap_sha256,
        foreground,
        None,
        capture_retry_evidence,
    )
}

fn write_screenshot_bitmap_with_quality(
    params: &CaptureScreenshotParams,
    output_path: PathBuf,
    format: CaptureScreenshotFormat,
    captured: synapse_capture::CapturedBgraBitmap,
    capture_backend: &str,
    _bitmap_sha256: String,
    foreground: Option<ForegroundContext>,
    jpeg_quality: Option<u8>,
    capture_retry_evidence: Option<CaptureRetryEvidence>,
) -> Result<CaptureScreenshotResponse, ErrorData> {
    let temp_path = screenshot_temp_path(&output_path);
    if temp_path.try_exists().map_err(|error| {
        mcp_error(
            error_codes::STORAGE_WRITE_FAILED,
            format!(
                "capture_screenshot temp path existence check failed for {}: {error}",
                temp_path.display()
            ),
        )
    })? {
        return Err(mcp_error(
            error_codes::STORAGE_WRITE_FAILED,
            format!(
                "capture_screenshot temp path already exists: {}",
                temp_path.display()
            ),
        ));
    }
    let source_region = captured.region;
    let native_width = captured.width;
    let native_height = captured.height;
    let (captured, scale) =
        downscale_captured_bitmap(captured, params.max_pixels, params.max_long_edge)?;
    save_screenshot_bitmap_with_quality(&captured, &temp_path, format, jpeg_quality)?;
    install_screenshot_file(&temp_path, &output_path, params.overwrite)?;
    let metadata = std::fs::metadata(&output_path).map_err(|error| {
        mcp_error(
            error_codes::STORAGE_READ_FAILED,
            format!(
                "capture_screenshot metadata readback failed for {}: {error}",
                output_path.display()
            ),
        )
    })?;
    if metadata.len() == 0 {
        return Err(mcp_error(
            error_codes::STORAGE_WRITE_FAILED,
            format!(
                "capture_screenshot wrote an empty file: {}",
                output_path.display()
            ),
        ));
    }
    let file_bytes = std::fs::read(&output_path).map_err(|error| {
        mcp_error(
            error_codes::STORAGE_READ_FAILED,
            format!(
                "capture_screenshot file hash readback failed for {}: {error}",
                output_path.display()
            ),
        )
    })?;
    if u64::try_from(file_bytes.len()).unwrap_or(u64::MAX) != metadata.len() {
        return Err(mcp_error(
            error_codes::STORAGE_READ_FAILED,
            format!(
                "capture_screenshot file readback length mismatch for {}: metadata={} read={}",
                output_path.display(),
                metadata.len(),
                file_bytes.len()
            ),
        ));
    }
    let bitmap_sha256 = sha256_hex(&file_bytes);
    Ok(CaptureScreenshotResponse {
        path: output_path.to_string_lossy().into_owned(),
        format,
        capture_backend: capture_backend.to_owned(),
        region: source_region,
        width: captured.width,
        height: captured.height,
        native_width,
        native_height,
        scale,
        bytes_written: metadata.len(),
        bitmap_sha256,
        foreground,
        capture_retry_evidence,
    })
}

/// Compute the aspect-preserving downscale factor for a `width`x`height` bitmap
/// so it fits within an optional `max_pixels` total-pixel budget and an optional
/// `max_long_edge` longest-edge budget. Returns the scale in `(0.0, 1.0]`; `1.0`
/// means no downscale is required. The more restrictive constraint wins. Loudly
/// rejects zero budgets so a caller never silently gets an un-scaled image.
fn screenshot_downscale_scale(
    width: u32,
    height: u32,
    max_pixels: Option<u64>,
    max_long_edge: Option<u32>,
) -> Result<f64, ErrorData> {
    if max_pixels == Some(0) {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "capture_screenshot max_pixels must be greater than zero",
        ));
    }
    if max_long_edge == Some(0) {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "capture_screenshot max_long_edge must be greater than zero",
        ));
    }
    if width == 0 || height == 0 {
        return Err(mcp_error(
            error_codes::CAPTURE_TARGET_INVALID,
            format!("capture_screenshot cannot scale an empty {width}x{height} bitmap"),
        ));
    }
    let mut scale = 1.0_f64;
    if let Some(max_long_edge) = max_long_edge {
        let long_edge = width.max(height);
        if long_edge > max_long_edge {
            scale = scale.min(f64::from(max_long_edge) / f64::from(long_edge));
        }
    }
    if let Some(max_pixels) = max_pixels {
        let pixels = u64::from(width) * u64::from(height);
        if pixels > max_pixels {
            // Area scales with the square of the linear factor.
            scale = scale.min((max_pixels as f64 / pixels as f64).sqrt());
        }
    }
    Ok(scale.min(1.0))
}

/// Downscale a captured BGRA bitmap (aspect-preserving) to fit the optional vision
/// pixel budget, returning the possibly-resized bitmap and the applied scale
/// (`written_long_edge / native_long_edge`). A scale of `1.0` returns the bitmap
/// untouched. Uses Lanczos3 resampling via the `image` crate already linked here.
fn downscale_captured_bitmap(
    captured: synapse_capture::CapturedBgraBitmap,
    max_pixels: Option<u64>,
    max_long_edge: Option<u32>,
) -> Result<(synapse_capture::CapturedBgraBitmap, f64), ErrorData> {
    let scale =
        screenshot_downscale_scale(captured.width, captured.height, max_pixels, max_long_edge)?;
    if scale >= 1.0 {
        return Ok((captured, 1.0));
    }
    let native_long_edge = captured.width.max(captured.height);
    let target_width = ((f64::from(captured.width) * scale).round() as u32).max(1);
    let target_height = ((f64::from(captured.height) * scale).round() as u32).max(1);
    // Build an RgbaImage from the BGRA source, resize, then swap back to BGRA so the
    // downstream encoder (which expects BGRA) keeps working unchanged.
    let mut rgba = captured.bytes;
    for pixel in rgba.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }
    let source = RgbaImage::from_raw(captured.width, captured.height, rgba).ok_or_else(|| {
        mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!(
                "capture_screenshot could not build image buffer from {}x{} bitmap before downscale",
                captured.width, captured.height
            ),
        )
    })?;
    let resized = image::imageops::resize(
        &source,
        target_width,
        target_height,
        image::imageops::FilterType::Lanczos3,
    );
    let resized_width = resized.width();
    let resized_height = resized.height();
    let mut bgra = resized.into_raw();
    for pixel in bgra.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }
    let applied_scale = f64::from(resized_width.max(resized_height)) / f64::from(native_long_edge);
    let bitmap = synapse_capture::CapturedBgraBitmap {
        region: bitmap_full_region(resized_width, resized_height)?,
        width: resized_width,
        height: resized_height,
        bytes: bgra,
    };
    Ok((bitmap, applied_scale))
}

fn hidden_desktop_pip_ended_response(
    params: &HiddenDesktopPipFrameParams,
    watched_session_id: &str,
    lifecycle: Option<String>,
    reason: &str,
) -> HiddenDesktopPipFrameResponse {
    HiddenDesktopPipFrameResponse {
        stream_status: HiddenDesktopPipStreamStatus::Ended,
        watched_session_id: watched_session_id.to_owned(),
        watched_session_lifecycle: lifecycle,
        watched_window_hwnd: params.window_hwnd,
        viewer_surface: "mcp_file_frame".to_owned(),
        read_only: true,
        input_forwarding: "none".to_owned(),
        desktop_names: Vec::new(),
        launch_pids: Vec::new(),
        resource_count: 0,
        ended_reason: Some(reason.to_owned()),
        path: None,
        format: None,
        capture_backend: None,
        region: None,
        width: None,
        height: None,
        bytes_written: None,
        bitmap_sha256: None,
        foreground: None,
    }
}

fn validate_screenshot_region(region: Rect) -> Result<(), ErrorData> {
    if region.w <= 0 || region.h <= 0 {
        return Err(mcp_error(
            error_codes::CAPTURE_TARGET_INVALID,
            format!(
                "capture_screenshot region must be non-empty: bbox=({}, {}, {}, {})",
                region.x, region.y, region.w, region.h
            ),
        ));
    }
    Ok(())
}

fn screenshot_output_path(raw_path: &str) -> Result<PathBuf, ErrorData> {
    let trimmed = raw_path.trim();
    if trimmed.is_empty() {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "capture_screenshot path must be a non-empty absolute file path",
        ));
    }
    let path = PathBuf::from(trimmed);
    if !path.is_absolute() {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "capture_screenshot path must be absolute: {}",
                path.display()
            ),
        ));
    }
    Ok(path)
}

fn browser_pdf_output_path(raw_path: &str) -> Result<PathBuf, ErrorData> {
    let path = screenshot_output_path(raw_path)?;
    let Some(extension) = path
        .extension()
        .and_then(|value| value.to_str())
        .map(str::to_ascii_lowercase)
    else {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("browser_pdf path must end in .pdf: {}", path.display()),
        ));
    };
    if extension != "pdf" {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!("browser_pdf unsupported file extension .{extension}; expected .pdf"),
        ));
    }
    Ok(path)
}

fn browser_download_output_path(raw_path: &str) -> Result<PathBuf, ErrorData> {
    let trimmed = raw_path.trim();
    if trimmed.is_empty() {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "browser_downloads path must be a non-empty absolute file path",
        ));
    }
    let path = PathBuf::from(trimmed);
    if !path.is_absolute() {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "browser_downloads path must be absolute: {}",
                path.display()
            ),
        ));
    }
    Ok(path)
}

fn screenshot_format_from_path(path: &Path) -> Result<CaptureScreenshotFormat, ErrorData> {
    let Some(extension) = path
        .extension()
        .and_then(|value| value.to_str())
        .map(str::to_ascii_lowercase)
    else {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "capture_screenshot path must end in .png, .jpg, or .jpeg: {}",
                path.display()
            ),
        ));
    };
    match extension.as_str() {
        "png" => Ok(CaptureScreenshotFormat::Png),
        "jpg" | "jpeg" => Ok(CaptureScreenshotFormat::Jpeg),
        other => Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "capture_screenshot unsupported file extension .{other}; expected .png, .jpg, or .jpeg"
            ),
        )),
    }
}

fn ensure_screenshot_path_available(path: &Path, overwrite: bool) -> Result<(), ErrorData> {
    if path.try_exists().map_err(|error| {
        mcp_error(
            error_codes::STORAGE_READ_FAILED,
            format!(
                "capture_screenshot output path existence check failed for {}: {error}",
                path.display()
            ),
        )
    })? {
        if path.is_dir() {
            return Err(mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                format!(
                    "capture_screenshot output path is a directory: {}",
                    path.display()
                ),
            ));
        }
        if !overwrite {
            return Err(mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                format!(
                    "capture_screenshot output file already exists and overwrite=false: {}",
                    path.display()
                ),
            ));
        }
    }
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent).map_err(|error| {
            mcp_error(
                error_codes::STORAGE_WRITE_FAILED,
                format!(
                    "capture_screenshot failed to create parent directory {}: {error}",
                    parent.display()
                ),
            )
        })?;
    }
    Ok(())
}

fn ensure_download_output_path_available(path: &Path, overwrite: bool) -> Result<(), ErrorData> {
    if path.try_exists().map_err(|error| {
        mcp_error(
            error_codes::STORAGE_READ_FAILED,
            format!(
                "browser_downloads output path existence check failed for {}: {error}",
                path.display()
            ),
        )
    })? {
        if path.is_dir() {
            return Err(mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                format!(
                    "browser_downloads output path is a directory: {}",
                    path.display()
                ),
            ));
        }
        if !overwrite {
            return Err(mcp_error(
                error_codes::TOOL_PARAMS_INVALID,
                format!(
                    "browser_downloads output file already exists and overwrite=false: {}",
                    path.display()
                ),
            ));
        }
    }
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent).map_err(|error| {
            mcp_error(
                error_codes::STORAGE_WRITE_FAILED,
                format!(
                    "browser_downloads failed to create parent directory {}: {error}",
                    parent.display()
                ),
            )
        })?;
    }
    Ok(())
}

fn save_screenshot_bitmap_with_quality(
    captured: &synapse_capture::CapturedBgraBitmap,
    path: &Path,
    format: CaptureScreenshotFormat,
    jpeg_quality: Option<u8>,
) -> Result<(), ErrorData> {
    let expected_len = usize::try_from(captured.width)
        .ok()
        .and_then(|width| {
            usize::try_from(captured.height)
                .ok()
                .and_then(|height| width.checked_mul(height))
        })
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| {
            mcp_error(
                error_codes::CAPTURE_TARGET_INVALID,
                format!(
                    "capture_screenshot bitmap dimensions overflow: {}x{}",
                    captured.width, captured.height
                ),
            )
        })?;
    if captured.bytes.len() != expected_len {
        return Err(mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!(
                "capture_screenshot BGRA byte length mismatch: expected {expected_len}, got {}",
                captured.bytes.len()
            ),
        ));
    }
    let mut rgba = captured.bytes.clone();
    for pixel in rgba.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }
    let image = RgbaImage::from_raw(captured.width, captured.height, rgba).ok_or_else(|| {
        mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            format!(
                "capture_screenshot could not create image buffer from {}x{} bitmap",
                captured.width, captured.height
            ),
        )
    })?;
    let result = match format {
        CaptureScreenshotFormat::Png => image.save_with_format(path, ImageFormat::Png),
        CaptureScreenshotFormat::Jpeg => {
            let rgb = DynamicImage::ImageRgba8(image).to_rgb8();
            let file = std::fs::File::create(path).map_err(|error| {
                mcp_error(
                    error_codes::STORAGE_WRITE_FAILED,
                    format!(
                        "capture_screenshot failed to create {}: {error}",
                        path.display()
                    ),
                )
            })?;
            let quality = jpeg_quality.unwrap_or(90);
            let mut encoder = JpegEncoder::new_with_quality(file, quality);
            return encoder.encode_image(&rgb).map_err(|error| {
                mcp_error(
                    error_codes::STORAGE_WRITE_FAILED,
                    format!(
                        "capture_screenshot failed to encode {}: {error}",
                        path.display()
                    ),
                )
            });
        }
    };
    result.map_err(|error| {
        mcp_error(
            error_codes::STORAGE_WRITE_FAILED,
            format!(
                "capture_screenshot failed to encode {}: {error}",
                path.display()
            ),
        )
    })
}

fn browser_download_source_path(selected: &BrowserDownloadEntry) -> Result<PathBuf, ErrorData> {
    if selected.filename.trim().is_empty() {
        return Err(mcp_error(
            error_codes::ACTION_POSTCONDITION_FAILED,
            format!(
                "browser_downloads selected download {} did not report a local filename",
                selected.id
            ),
        ));
    }
    let path = PathBuf::from(selected.filename.trim());
    if !path.is_absolute() {
        return Err(mcp_error(
            error_codes::ACTION_POSTCONDITION_FAILED,
            format!(
                "browser_downloads selected download {} filename is not absolute: {}",
                selected.id,
                path.display()
            ),
        ));
    }
    if selected.exists == Some(false) {
        return Err(mcp_error(
            error_codes::ACTION_POSTCONDITION_FAILED,
            format!(
                "browser_downloads selected download {} reports exists=false: {}",
                selected.id,
                path.display()
            ),
        ));
    }
    Ok(path)
}

fn copy_or_move_download_file(
    source_path: &Path,
    output_path: &Path,
    overwrite: bool,
    move_file: bool,
) -> Result<(u64, String), ErrorData> {
    let source_metadata = std::fs::metadata(source_path).map_err(|error| {
        mcp_error(
            error_codes::STORAGE_READ_FAILED,
            format!(
                "browser_downloads source metadata readback failed for {}: {error}",
                source_path.display()
            ),
        )
    })?;
    if source_metadata.is_dir() {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "browser_downloads source path is a directory: {}",
                source_path.display()
            ),
        ));
    }
    ensure_download_output_path_available(output_path, overwrite)?;
    let temp_path = screenshot_temp_path(output_path);
    if temp_path.try_exists().map_err(|error| {
        mcp_error(
            error_codes::STORAGE_READ_FAILED,
            format!(
                "browser_downloads temp path existence check failed for {}: {error}",
                temp_path.display()
            ),
        )
    })? {
        return Err(mcp_error(
            error_codes::STORAGE_WRITE_FAILED,
            format!(
                "browser_downloads temp path already exists: {}",
                temp_path.display()
            ),
        ));
    }
    std::fs::copy(source_path, &temp_path).map_err(|error| {
        mcp_error(
            error_codes::STORAGE_WRITE_FAILED,
            format!(
                "browser_downloads failed to copy {} to {}: {error}",
                source_path.display(),
                temp_path.display()
            ),
        )
    })?;
    install_download_file(&temp_path, output_path, overwrite)?;
    let (saved_bytes, saved_sha256) = sha256_file(output_path)?;
    if saved_bytes != source_metadata.len() {
        return Err(mcp_error(
            error_codes::ACTION_POSTCONDITION_FAILED,
            format!(
                "browser_downloads byte-count mismatch after save: source={} output={}",
                source_metadata.len(),
                saved_bytes
            ),
        ));
    }
    if move_file {
        std::fs::remove_file(source_path).map_err(|error| {
            mcp_error(
                error_codes::STORAGE_WRITE_FAILED,
                format!(
                    "browser_downloads saved {}, but failed to remove source {} for move: {error}",
                    output_path.display(),
                    source_path.display()
                ),
            )
        })?;
    }
    Ok((saved_bytes, saved_sha256))
}

fn install_download_file(
    temp_path: &Path,
    output_path: &Path,
    overwrite: bool,
) -> Result<(), ErrorData> {
    if overwrite && output_path.exists() {
        std::fs::remove_file(output_path).map_err(|error| {
            let _ = std::fs::remove_file(temp_path);
            mcp_error(
                error_codes::STORAGE_WRITE_FAILED,
                format!(
                    "browser_downloads failed to replace existing file {}: {error}",
                    output_path.display()
                ),
            )
        })?;
    }
    std::fs::rename(temp_path, output_path).map_err(|error| {
        let _ = std::fs::remove_file(temp_path);
        mcp_error(
            error_codes::STORAGE_WRITE_FAILED,
            format!(
                "browser_downloads failed to move {} to {}: {error}",
                temp_path.display(),
                output_path.display()
            ),
        )
    })
}

fn sha256_file(path: &Path) -> Result<(u64, String), ErrorData> {
    let mut file = std::fs::File::open(path).map_err(|error| {
        mcp_error(
            error_codes::STORAGE_READ_FAILED,
            format!(
                "browser_downloads failed to open {}: {error}",
                path.display()
            ),
        )
    })?;
    let mut hasher = Sha256::new();
    let mut buf = [0_u8; 64 * 1024];
    let mut total = 0_u64;
    loop {
        let read = file.read(&mut buf).map_err(|error| {
            mcp_error(
                error_codes::STORAGE_READ_FAILED,
                format!(
                    "browser_downloads failed to read {}: {error}",
                    path.display()
                ),
            )
        })?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
        total = total.saturating_add(read as u64);
    }
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(&mut hex, "{byte:02x}");
    }
    Ok((total, hex))
}

fn install_screenshot_file(
    temp_path: &Path,
    output_path: &Path,
    overwrite: bool,
) -> Result<(), ErrorData> {
    if overwrite && output_path.exists() {
        std::fs::remove_file(output_path).map_err(|error| {
            let _ = std::fs::remove_file(temp_path);
            mcp_error(
                error_codes::STORAGE_WRITE_FAILED,
                format!(
                    "capture_screenshot failed to replace existing file {}: {error}",
                    output_path.display()
                ),
            )
        })?;
    }
    std::fs::rename(temp_path, output_path).map_err(|error| {
        let _ = std::fs::remove_file(temp_path);
        mcp_error(
            error_codes::STORAGE_WRITE_FAILED,
            format!(
                "capture_screenshot failed to move {} to {}: {error}",
                temp_path.display(),
                output_path.display()
            ),
        )
    })
}

fn write_pdf_bytes(
    output_path: &Path,
    pdf_bytes: &[u8],
    overwrite: bool,
) -> Result<u64, ErrorData> {
    if pdf_bytes.is_empty() {
        return Err(mcp_error(
            error_codes::STORAGE_WRITE_FAILED,
            format!(
                "browser_pdf refused to write empty PDF: {}",
                output_path.display()
            ),
        ));
    }
    let temp_path = screenshot_temp_path(output_path);
    if temp_path.try_exists().map_err(|error| {
        mcp_error(
            error_codes::STORAGE_READ_FAILED,
            format!(
                "browser_pdf temp path existence check failed for {}: {error}",
                temp_path.display()
            ),
        )
    })? {
        return Err(mcp_error(
            error_codes::STORAGE_WRITE_FAILED,
            format!(
                "browser_pdf temp path already exists: {}",
                temp_path.display()
            ),
        ));
    }
    std::fs::write(&temp_path, pdf_bytes).map_err(|error| {
        mcp_error(
            error_codes::STORAGE_WRITE_FAILED,
            format!(
                "browser_pdf failed to write {}: {error}",
                temp_path.display()
            ),
        )
    })?;
    install_screenshot_file(&temp_path, output_path, overwrite)?;
    let metadata = std::fs::metadata(output_path).map_err(|error| {
        mcp_error(
            error_codes::STORAGE_READ_FAILED,
            format!(
                "browser_pdf metadata readback failed for {}: {error}",
                output_path.display()
            ),
        )
    })?;
    if metadata.len() == 0 {
        return Err(mcp_error(
            error_codes::STORAGE_WRITE_FAILED,
            format!("browser_pdf wrote an empty file: {}", output_path.display()),
        ));
    }
    Ok(metadata.len())
}

fn screenshot_temp_path(output_path: &Path) -> PathBuf {
    let now_ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    let file_name = output_path
        .file_name()
        .map(|value| value.to_string_lossy())
        .unwrap_or_else(|| "capture".into());
    output_path.with_file_name(format!(
        ".{file_name}.{}.{}.tmp",
        std::process::id(),
        now_ns
    ))
}

impl SynapseService {
    pub(super) fn resolve_input_profile_and_hud(
        &self,
        input: &mut synapse_perception::ObservationInput,
        include_hud: bool,
    ) {
        match self.reevaluate_profile_for_foreground(&input.foreground) {
            Ok(transition) => {
                let Some(profile_id) = transition.active_profile_id.clone() else {
                    tracing::debug!(
                        code = "PROFILE_FOREGROUND_UNMATCHED",
                        "observed foreground did not match a loaded profile"
                    );
                    return;
                };
                tracing::info!(
                    code = "PROFILE_FOREGROUND_MATCHED",
                    profile_id = %profile_id,
                    rank = ?transition.resolution.as_ref().map(|resolution| resolution.rank_name),
                    "observed foreground matched profile"
                );
                input.foreground.profile_id = Some(profile_id.clone());
                let Ok(runtime) = self.profile_runtime() else {
                    tracing::warn!(
                        code = "PROFILE_FOREGROUND_RESOLUTION_SKIPPED",
                        "profile runtime unavailable while resolving observed foreground profile config"
                    );
                    return;
                };
                match runtime.profile(&profile_id) {
                    Ok(Some(profile)) => {
                        if let Err(error) = self.apply_m1_runtime_config_for_profile(&profile) {
                            tracing::warn!(
                                code = "PROFILE_M1_RUNTIME_CONFIG_FAILED",
                                profile_id = %profile_id,
                                error = %error,
                                "profile runtime config failed for observed foreground"
                            );
                        } else if let Ok(state) = self.m1_state() {
                            input.mode_override = Some(state.perception_mode);
                            input.capture_config = Some(state.active_capture_config.clone());
                            input.capture_runtime = Some(state.capture_runtime_readback());
                        }
                        if include_hud {
                            populate_profile_hud(input, &profile, runtime.profile_dir());
                        }
                    }
                    Ok(None) => {
                        tracing::warn!(
                            code = "PROFILE_HUD_PROFILE_MISSING",
                            profile_id = %profile_id,
                            "profile resolved but could not be loaded for HUD extraction"
                        );
                    }
                    Err(error) => {
                        tracing::warn!(
                            code = "PROFILE_HUD_PROFILE_LOAD_FAILED",
                            profile_id = %profile_id,
                            error = %error,
                            "profile load failed for HUD extraction"
                        );
                    }
                }
            }
            Err(error) => {
                tracing::warn!(
                    code = "PROFILE_FOREGROUND_RESOLUTION_FAILED",
                    error = %error,
                    "profile resolver failed for observed foreground"
                );
            }
        }
    }

    fn populate_input_backend_diagnostics(&self, input: &mut synapse_perception::ObservationInput) {
        let Ok(state) = self.m2_state.lock() else {
            input.input_backends = Some(input_backend_diagnostics_from_error(
                "m2_state_lock_poisoned",
                error_codes::TOOL_INTERNAL_ERROR,
                "M2 service state lock poisoned",
            ));
            return;
        };
        match state.backend_resolution_readback() {
            Ok((source, policy)) => {
                input.input_backends = Some(input_backend_diagnostics(&source, policy));
            }
            Err(error) => {
                input.input_backends = Some(input_backend_diagnostics_from_error(
                    "backend_resolution_unavailable",
                    error_codes::TOOL_INTERNAL_ERROR,
                    error,
                ));
            }
        }
    }
}

impl SynapseService {
    /// OCRs a CDP/web element by capturing its rendered pixels via CDP and
    /// running WinRT OCR on them (#703). UIA element-bounds resolution cannot see
    /// web nodes, so `read_text(element_id=<web node>)` routes here. Fail-loud if
    /// the browser/debug endpoint is gone or the node has no rendered box.
    #[cfg(windows)]
    async fn read_text_web_element(
        &self,
        element_id: &synapse_core::ElementId,
        backend_node_id: i64,
        params: &ReadTextParams,
    ) -> Result<OcrResult, ErrorData> {
        let hwnd = element_id
            .parts()
            .map_err(|err| {
                mcp_error(
                    error_codes::ACTION_ELEMENT_NOT_RESOLVED,
                    format!("web element id is malformed: {err}"),
                )
            })?
            .hwnd;
        let endpoint = synapse_a11y::endpoint_for_window(hwnd).ok_or_else(|| {
            mcp_error(
                error_codes::A11Y_CDP_UNREACHABLE,
                format!(
                    "no reachable CDP endpoint for web element {element_id} (browser closed or debug port gone)"
                ),
            )
        })?;
        let title_hint = synapse_a11y::foreground_context(hwnd)
            .map(|context| context.window_title)
            .unwrap_or_default();
        let target_id_hint = synapse_a11y::cdp_target_from_element_id(element_id);
        let bitmap = synapse_a11y::cdp_capture_node_bgra(
            &endpoint,
            &title_hint,
            target_id_hint.as_deref(),
            backend_node_id,
        )
        .await
        .map_err(|err| {
            mcp_error(
                err.code(),
                format!("web element OCR capture failed for {element_id}: {err}"),
            )
        })?;
        crate::m1::ocr_result_from_web_bitmap(
            bitmap.width,
            bitmap.height,
            &bitmap.bgra,
            params.lang_hint.as_deref(),
        )
    }

    #[cfg(windows)]
    fn read_text_request_with_cache(
        &self,
        request: crate::m1::ResolvedReadTextRequest,
    ) -> Result<OcrResult, ErrorData> {
        if request.synthetic || request.effective_backend != OcrBackend::Winrt {
            return read_text_request_uncached(&request);
        }

        let captured = capture_ocr_bitmap(&request)?;
        self.read_text_request_with_captured_bitmap(request, captured)
    }

    #[cfg(windows)]
    fn read_text_request_with_captured_bitmap(
        &self,
        request: crate::m1::ResolvedReadTextRequest,
        captured: CapturedOcrBitmap,
    ) -> Result<OcrResult, ErrorData> {
        let request = crate::m1::read_text_request_for_captured_bitmap(request, &captured)?;
        if request.synthetic || request.effective_backend != OcrBackend::Winrt {
            return read_text_request_uncached(&request);
        }
        let bitmap_sha256 = sha256_hex(&captured.bytes);
        let cache_key = ocr_cache_key(
            &request,
            captured.width,
            captured.height,
            &bitmap_sha256,
            captured.capture_backend,
        );
        let runtime = self.reflex_runtime()?;

        {
            let runtime = lock_reflex_runtime(&runtime)?;
            if let Some(row) = read_ocr_cache_row(
                &runtime,
                &cache_key,
                &request,
                captured.width,
                captured.height,
                &bitmap_sha256,
                &captured,
            )? {
                tracing::info!(
                    code = "OCR_CACHE_HIT",
                    cache_key = %cache_key,
                    backend = ocr_backend_name(request.effective_backend),
                    region_x = request.region.x,
                    region_y = request.region.y,
                    region_w = request.region.w,
                    region_h = request.region.h,
                    word_count = row.word_count,
                    recognition_latency_ms = row.recognition_latency_ms,
                    "OCR cache hit"
                );
                crate::m3::hygiene::scan_and_persist_ocr_result(
                    &runtime,
                    &row.result,
                    cache_key.as_bytes(),
                )?;
                return Ok(row.result);
            }
        }

        let recognition_start = Instant::now();
        let result = crate::m1::read_text_request_from_bgra(&request, &captured)?;
        let recognition_latency_ms = elapsed_ms_u64(recognition_start);
        let row = OcrCacheRow {
            schema_version: SCHEMA_VERSION,
            cache_key: cache_key.clone(),
            created_at: Utc::now(),
            requested_backend: request.requested_backend,
            effective_backend: request.effective_backend,
            lang: request.lang(),
            region: request.region,
            capture_source: captured.capture_source.to_owned(),
            capture_backend: captured.capture_backend.to_owned(),
            capture_hwnd: captured.capture_hwnd,
            capture_region: captured.capture_region,
            bitmap_sha256: bitmap_sha256.clone(),
            bitmap_width: captured.width,
            bitmap_height: captured.height,
            bitmap_bytes: captured.bytes.len() as u64,
            result: result.clone(),
            recognition_latency_ms,
            word_count: result.words.len() as u64,
        };
        let encoded = encode_json(&row).map_err(|error| {
            mcp_error(
                error.code(),
                format!("OCR cache row encode failed for key {cache_key}: {error}"),
            )
        })?;
        {
            let runtime = lock_reflex_runtime(&runtime)?;
            if !runtime.storage_pressure_permits_write(cf::CF_OCR_CACHE) {
                return Err(mcp_error(
                    error_codes::STORAGE_WRITE_FAILED,
                    format!(
                        "OCR cache write refused under disk pressure: cf_name={} key={cache_key}",
                        cf::CF_OCR_CACHE
                    ),
                ));
            }
            runtime
                .storage_put_rows(
                    cf::CF_OCR_CACHE,
                    vec![(cache_key.as_bytes().to_vec(), encoded)],
                )
                .map_err(|error| {
                    mcp_error(
                        error.code(),
                        format!("OCR cache write failed for key {cache_key}: {error}"),
                    )
                })?;
            let readback = read_ocr_cache_row(
                &runtime,
                &cache_key,
                &request,
                captured.width,
                captured.height,
                &bitmap_sha256,
                &captured,
            )?
            .ok_or_else(|| {
                mcp_error(
                    error_codes::STORAGE_WRITE_FAILED,
                    format!("OCR cache write had no readback row: key={cache_key}"),
                )
            })?;
            if readback.result != result {
                return Err(mcp_error(
                    error_codes::STORAGE_WRITE_FAILED,
                    format!("OCR cache readback result mismatch for key {cache_key}"),
                ));
            }
            crate::m3::hygiene::scan_and_persist_ocr_result(
                &runtime,
                &readback.result,
                cache_key.as_bytes(),
            )?;
        }

        tracing::info!(
            code = "OCR_CACHE_MISS_RECORDED",
            cache_key = %cache_key,
            backend = ocr_backend_name(request.effective_backend),
            region_x = request.region.x,
            region_y = request.region.y,
            region_w = request.region.w,
            region_h = request.region.h,
            word_count = result.words.len(),
            recognition_latency_ms,
            "OCR cache miss recorded"
        );
        Ok(result)
    }

    #[cfg(not(windows))]
    fn read_text_request_with_cache(
        &self,
        request: crate::m1::ResolvedReadTextRequest,
    ) -> Result<OcrResult, ErrorData> {
        read_text_request_uncached(&request)
    }
}

#[cfg(windows)]
struct CapturedOcrBitmap {
    bitmap: synapse_capture::CapturedBgraBitmap,
    capture_source: &'static str,
    capture_backend: &'static str,
    capture_hwnd: Option<i64>,
    capture_region: Rect,
}

#[cfg(windows)]
impl std::ops::Deref for CapturedOcrBitmap {
    type Target = synapse_capture::CapturedBgraBitmap;

    fn deref(&self) -> &Self::Target {
        &self.bitmap
    }
}

#[cfg(windows)]
fn capture_ocr_bitmap(
    request: &crate::m1::ResolvedReadTextRequest,
) -> Result<CapturedOcrBitmap, ErrorData> {
    match request.capture_source {
        crate::m1::ReadTextCaptureSource::Screen => {
            let bitmap =
                synapse_capture::screen_region_to_bgra_bitmap(request.region).map_err(|error| {
                    mcp_error(
                        error.code(),
                        format!(
                            "OCR screen capture failed for region {:?}: {error}",
                            request.region
                        ),
                    )
                })?;
            Ok(CapturedOcrBitmap {
                bitmap,
                capture_source: "screen",
                capture_backend: "gdi_screen_region_bgra",
                capture_hwnd: None,
                capture_region: request.region,
            })
        }
        crate::m1::ReadTextCaptureSource::Window {
            hwnd,
            window_region,
        } => {
            let captured = synapse_capture::window_region_to_bgra_bitmap(
                hwnd,
                window_region,
                WINDOW_SCREENSHOT_TIMEOUT_MS,
            )
            .map_err(|error| {
                mcp_error(
                    error.code(),
                    format!(
                        "OCR target-window capture failed for hwnd {hwnd:#x} region {window_region:?}: {error}"
                    ),
                )
            })?;
            Ok(CapturedOcrBitmap {
                bitmap: captured.bitmap,
                capture_source: "window",
                capture_backend: captured.capture_backend,
                capture_hwnd: Some(hwnd),
                capture_region: window_region,
            })
        }
        crate::m1::ReadTextCaptureSource::WholeWindow { hwnd } => {
            let captured = synapse_capture::window_full_frame_to_bgra_bitmap(
                hwnd,
                WINDOW_SCREENSHOT_TIMEOUT_MS,
            )
            .map_err(|error| {
                mcp_error(
                    error.code(),
                    format!("OCR whole-window capture failed for hwnd {hwnd:#x}: {error}"),
                )
            })?;
            let capture_region = full_bitmap_region(&captured.bitmap)?;
            Ok(CapturedOcrBitmap {
                bitmap: captured.bitmap,
                capture_source: "whole_window",
                capture_backend: captured.capture_backend,
                capture_hwnd: Some(hwnd),
                capture_region,
            })
        }
    }
}

#[cfg(windows)]
fn full_bitmap_region(bitmap: &synapse_capture::CapturedBgraBitmap) -> Result<Rect, ErrorData> {
    Ok(Rect {
        x: 0,
        y: 0,
        w: i32::try_from(bitmap.width).map_err(|_| {
            mcp_error(
                error_codes::OCR_NO_TEXT,
                format!("OCR bitmap width {} exceeds i32", bitmap.width),
            )
        })?,
        h: i32::try_from(bitmap.height).map_err(|_| {
            mcp_error(
                error_codes::OCR_NO_TEXT,
                format!("OCR bitmap height {} exceeds i32", bitmap.height),
            )
        })?,
    })
}

#[cfg(windows)]
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct OcrCacheRow {
    schema_version: u32,
    cache_key: String,
    created_at: DateTime<Utc>,
    requested_backend: OcrBackend,
    effective_backend: OcrBackend,
    lang: String,
    region: Rect,
    capture_source: String,
    capture_backend: String,
    capture_hwnd: Option<i64>,
    capture_region: Rect,
    bitmap_sha256: String,
    bitmap_width: u32,
    bitmap_height: u32,
    bitmap_bytes: u64,
    result: OcrResult,
    recognition_latency_ms: u64,
    word_count: u64,
}

#[cfg(windows)]
fn read_ocr_cache_row(
    runtime: &ReflexRuntime,
    cache_key: &str,
    request: &crate::m1::ResolvedReadTextRequest,
    bitmap_width: u32,
    bitmap_height: u32,
    bitmap_sha256: &str,
    captured: &CapturedOcrBitmap,
) -> Result<Option<OcrCacheRow>, ErrorData> {
    let rows = runtime
        .storage_cf_prefix_rows(cf::CF_OCR_CACHE, cache_key.as_bytes(), 1)
        .map_err(|error| {
            mcp_error(
                error.code(),
                format!("OCR cache read failed for key {cache_key}: {error}"),
            )
        })?;
    let Some((row_key, value)) = rows
        .into_iter()
        .find(|(row_key, _value)| row_key.as_slice() == cache_key.as_bytes())
    else {
        return Ok(None);
    };
    let row = decode_json::<OcrCacheRow>(&value).map_err(|error| {
        mcp_error(
            error.code(),
            format!("OCR cache row decode failed for key {cache_key}: {error}"),
        )
    })?;
    if !valid_ocr_cache_row(
        &row,
        cache_key,
        request,
        bitmap_width,
        bitmap_height,
        bitmap_sha256,
        captured,
    ) {
        tracing::warn!(
            code = "OCR_CACHE_ROW_INVALID",
            cache_key = %cache_key,
            row_key = %String::from_utf8_lossy(&row_key),
            "OCR cache row failed validation and will be ignored"
        );
        return Ok(None);
    }
    Ok(Some(row))
}

#[cfg(windows)]
fn valid_ocr_cache_row(
    row: &OcrCacheRow,
    cache_key: &str,
    request: &crate::m1::ResolvedReadTextRequest,
    bitmap_width: u32,
    bitmap_height: u32,
    bitmap_sha256: &str,
    captured: &CapturedOcrBitmap,
) -> bool {
    row.schema_version == SCHEMA_VERSION
        && row.cache_key == cache_key
        && row.requested_backend == request.requested_backend
        && row.effective_backend == request.effective_backend
        && row.lang == request.lang()
        && row.region == request.region
        && row.capture_source == captured.capture_source
        && row.capture_backend == captured.capture_backend
        && row.capture_hwnd == captured.capture_hwnd
        && row.capture_region == captured.capture_region
        && row.bitmap_width == bitmap_width
        && row.bitmap_height == bitmap_height
        && row.bitmap_sha256 == bitmap_sha256
        && row.result.region == request.region
}

#[cfg(windows)]
fn ocr_cache_key(
    request: &crate::m1::ResolvedReadTextRequest,
    bitmap_width: u32,
    bitmap_height: u32,
    bitmap_sha256: &str,
    capture_backend: &str,
) -> String {
    format!(
        "ocr/cache/v2/{}/{}/{}/{}/{}/{}/{}/{}/{}/{}/{}",
        ocr_backend_name(request.requested_backend),
        ocr_backend_name(request.effective_backend),
        sha256_hex(request.lang().as_bytes()),
        capture_backend,
        request.region.x,
        request.region.y,
        request.region.w,
        request.region.h,
        bitmap_width,
        bitmap_height,
        bitmap_sha256
    )
}

#[cfg(windows)]
fn lock_reflex_runtime(
    runtime: &std::sync::Arc<std::sync::Mutex<ReflexRuntime>>,
) -> Result<std::sync::MutexGuard<'_, ReflexRuntime>, ErrorData> {
    runtime.lock().map_err(|_error| {
        mcp_error(
            error_codes::TOOL_INTERNAL_ERROR,
            "reflex runtime lock poisoned while accessing OCR cache",
        )
    })
}

#[cfg(windows)]
fn elapsed_ms_u64(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX)
}

const CDP_TARGET_INFO_PAGE_TEXT_MAX_CHARS: usize = 4096;
const CDP_TARGET_INFO_PAGE_VITALS_SCRIPT: &str = r##"
(() => {
  function stringValue(value) {
    return value === null || value === undefined ? "" : String(value);
  }
  function numberValue(value) {
    return typeof value === "number" && Number.isFinite(value) ? value : 0;
  }
  function trimText(value, maxChars) {
    const normalized = stringValue(value).replace(/\s+/g, " ").trim();
    return Array.from(normalized).slice(0, maxChars).join("");
  }
  function cssEscape(value) {
    if (typeof CSS !== "undefined" && typeof CSS.escape === "function") {
      return CSS.escape(value);
    }
    return stringValue(value).replace(/["\\#.:,[\]>+~*'()=]/g, "\\$&");
  }
  function elementSelector(element) {
    if (!element || element.nodeType !== 1) {
      return null;
    }
    const parts = [];
    let current = element;
    while (current && current.nodeType === 1 && parts.length < 6) {
      const local = stringValue(current.localName || current.tagName).toLowerCase();
      let part = local || "element";
      if (current.id) {
        part += "#" + cssEscape(current.id);
        parts.unshift(part);
        break;
      }
      const classes = Array.from(current.classList || []).slice(0, 2);
      for (const className of classes) {
        part += "." + cssEscape(className);
      }
      const parent = current.parentElement;
      if (parent) {
        const siblings = Array.from(parent.children || []).filter((node) => node.localName === current.localName);
        if (siblings.length > 1) {
          part += `:nth-of-type(${siblings.indexOf(current) + 1})`;
        }
      }
      parts.unshift(part);
      current = parent;
    }
    return parts.join(" > ");
  }
  function lcpSummary(entry) {
    const element = entry && entry.element && entry.element.nodeType === 1 ? entry.element : null;
    return {
      name: stringValue(entry && entry.name),
      entry_type: stringValue(entry && entry.entryType) || "largest-contentful-paint",
      start_time: numberValue(entry && entry.startTime),
      render_time: numberValue(entry && entry.renderTime),
      load_time: numberValue(entry && entry.loadTime),
      size: numberValue(entry && entry.size),
      element_tag_name: element ? stringValue(element.tagName).toLowerCase() : null,
      element_id: element ? stringValue(element.id) : null,
      element_class_name: element ? stringValue(element.className) : null,
      element_selector: element ? elementSelector(element) : null,
      element_text: element ? trimText(element.innerText || element.textContent || "", 2048) : null,
      element_current_src: element && "currentSrc" in element ? stringValue(element.currentSrc) : null,
      element_url: element && "url" in entry ? stringValue(entry.url) : null
    };
  }
  try {
    const supported = Boolean(
      typeof PerformanceObserver !== "undefined" &&
        Array.isArray(PerformanceObserver.supportedEntryTypes) &&
        PerformanceObserver.supportedEntryTypes.includes("largest-contentful-paint")
    );
    const entries = supported && performance && typeof performance.getEntriesByType === "function"
      ? performance.getEntriesByType("largest-contentful-paint")
      : [];
    const last = entries.length > 0 ? entries[entries.length - 1] : null;
    return {
      available: true,
      readback_source: "PerformanceObserver.supportedEntryTypes+performance.getEntriesByType",
      visibility_state: stringValue(document.visibilityState),
      document_hidden: Boolean(document.hidden),
      ready_state: stringValue(document.readyState),
      lcp_supported: supported,
      lcp_entry_count: entries.length,
      lcp: last ? lcpSummary(last) : null,
      error_code: null,
      error_detail: null
    };
  } catch (error) {
    return {
      available: false,
      readback_source: "PerformanceObserver.supportedEntryTypes+performance.getEntriesByType",
      visibility_state: typeof document !== "undefined" ? stringValue(document.visibilityState) : null,
      document_hidden: typeof document !== "undefined" ? Boolean(document.hidden) : null,
      ready_state: typeof document !== "undefined" ? stringValue(document.readyState) : null,
      lcp_supported: false,
      lcp_entry_count: 0,
      lcp: null,
      error_code: "PAGE_VITALS_READ_FAILED",
      error_detail: error && error.message ? String(error.message) : String(error)
    };
  }
})()
"##;

#[cfg(windows)]
async fn raw_cdp_page_text_info(endpoint: &str, target_id: &str) -> Option<CdpPageTextInfo> {
    Some(
        match synapse_a11y::cdp_page_text_target(
            endpoint,
            target_id,
            CDP_TARGET_INFO_PAGE_TEXT_MAX_CHARS,
        )
        .await
        {
            Ok(readback) => raw_cdp_page_text_readback_info(readback),
            Err(error) => unavailable_page_text_info(
                "Runtime.evaluate",
                error.code(),
                error.to_string(),
                CDP_TARGET_INFO_PAGE_TEXT_MAX_CHARS,
            ),
        },
    )
}

#[cfg(windows)]
async fn raw_cdp_page_vitals_info(endpoint: &str, target_id: &str) -> Option<CdpPageVitalsInfo> {
    Some(
        match synapse_a11y::cdp_evaluate_expression(
            endpoint,
            target_id,
            CDP_TARGET_INFO_PAGE_VITALS_SCRIPT,
            true,
            true,
        )
        .await
        {
            Ok(readback) => match serde_json::from_value::<
                crate::chrome_debugger_bridge::ChromeDebuggerPageVitals,
            >(readback.value)
            {
                Ok(page_vitals) => chrome_page_vitals_info(&page_vitals),
                Err(error) => unavailable_page_vitals_info(
                    "Runtime.evaluate",
                    error_codes::A11Y_CDP_AXTREE_FAILED,
                    format!("Runtime.evaluate page vitals decode failed: {error}"),
                ),
            },
            Err(error) => {
                unavailable_page_vitals_info("Runtime.evaluate", error.code(), error.to_string())
            }
        },
    )
}

#[cfg(windows)]
fn raw_cdp_page_text_readback_info(readback: synapse_a11y::CdpPageTextState) -> CdpPageTextInfo {
    page_text_info_from_parts(
        true,
        "Runtime.evaluate",
        Some(readback.text),
        readback.text_len,
        readback.text_truncated,
        readback.max_chars,
        None,
        None,
    )
}

#[cfg(windows)]
fn chrome_page_vitals_info(
    page_vitals: &crate::chrome_debugger_bridge::ChromeDebuggerPageVitals,
) -> CdpPageVitalsInfo {
    CdpPageVitalsInfo {
        available: page_vitals.available,
        readback_source: if page_vitals.readback_source.trim().is_empty() {
            "chrome.scripting.executeScript"
        } else {
            page_vitals.readback_source.as_str()
        }
        .to_owned(),
        visibility_state: non_empty_string(page_vitals.visibility_state.as_deref()),
        document_hidden: page_vitals.document_hidden,
        ready_state: non_empty_string(page_vitals.ready_state.as_deref()),
        lcp_supported: page_vitals.lcp_supported,
        lcp_entry_count: page_vitals.lcp_entry_count,
        lcp: page_vitals
            .lcp
            .as_ref()
            .map(chrome_largest_contentful_paint_info),
        error_code: page_vitals.error_code.clone(),
        error_detail_sha256: page_vitals
            .error_detail
            .as_deref()
            .and_then(non_empty_text_sha256),
    }
}

#[cfg(windows)]
fn chrome_largest_contentful_paint_info(
    lcp: &crate::chrome_debugger_bridge::ChromeDebuggerLargestContentfulPaint,
) -> CdpLargestContentfulPaintInfo {
    CdpLargestContentfulPaintInfo {
        name: lcp.name.clone(),
        entry_type: if lcp.entry_type.trim().is_empty() {
            "largest-contentful-paint".to_owned()
        } else {
            lcp.entry_type.clone()
        },
        start_time: lcp.start_time,
        render_time: lcp.render_time,
        load_time: lcp.load_time,
        size: lcp.size,
        element_tag_name: non_empty_string(lcp.element_tag_name.as_deref()),
        element_id: non_empty_string(lcp.element_id.as_deref()),
        element_class_name: non_empty_string(lcp.element_class_name.as_deref()),
        element_selector: non_empty_string(lcp.element_selector.as_deref()),
        element_text_len: lcp.element_text.as_ref().map(|text| text.chars().count()),
        element_text_sha256: lcp.element_text.as_deref().and_then(non_empty_text_sha256),
        element_current_src_sha256: lcp
            .element_current_src
            .as_deref()
            .and_then(non_empty_text_sha256),
        element_url_sha256: lcp.element_url.as_deref().and_then(non_empty_text_sha256),
    }
}

#[cfg(windows)]
fn unavailable_page_vitals_info(
    readback_source: &str,
    error_code: impl Into<String>,
    error_detail: impl Into<String>,
) -> CdpPageVitalsInfo {
    CdpPageVitalsInfo {
        available: false,
        readback_source: readback_source.to_owned(),
        visibility_state: None,
        document_hidden: None,
        ready_state: None,
        lcp_supported: None,
        lcp_entry_count: 0,
        lcp: None,
        error_code: Some(error_code.into()),
        error_detail_sha256: non_empty_text_sha256(&error_detail.into()),
    }
}

#[cfg(windows)]
fn chrome_page_text_info(
    page_text: &crate::chrome_debugger_bridge::ChromeDebuggerPageText,
) -> CdpPageTextInfo {
    page_text_info_from_parts(
        page_text.available,
        if page_text.readback_source.trim().is_empty() {
            "chrome.scripting.executeScript"
        } else {
            &page_text.readback_source
        },
        page_text.text.clone(),
        page_text.text_len,
        page_text.text_truncated,
        page_text.max_chars,
        page_text.error_code.clone(),
        page_text.error_detail.clone(),
    )
}

#[cfg(windows)]
fn unavailable_page_text_info(
    readback_source: &str,
    error_code: impl Into<String>,
    error_detail: impl Into<String>,
    max_chars: usize,
) -> CdpPageTextInfo {
    page_text_info_from_parts(
        false,
        readback_source,
        None,
        0,
        false,
        max_chars,
        Some(error_code.into()),
        Some(error_detail.into()),
    )
}

#[cfg(windows)]
fn page_text_info_from_parts(
    available: bool,
    readback_source: &str,
    text: Option<String>,
    text_len: usize,
    text_truncated: bool,
    max_chars: usize,
    error_code: Option<String>,
    error_detail: Option<String>,
) -> CdpPageTextInfo {
    let mut info = CdpPageTextInfo {
        available,
        readback_source: readback_source.to_owned(),
        text_sha256: text.as_ref().map(|value| sha256_hex(value.as_bytes())),
        text,
        text_len,
        text_truncated,
        max_chars,
        error_code,
        error_detail_sha256: error_detail.as_deref().and_then(non_empty_text_sha256),
        perceived_text_notice: None,
        suspected_injection: Vec::new(),
    };
    attach_page_text_hygiene_annotations(&mut info);
    info
}

#[cfg(windows)]
fn attach_page_text_hygiene_annotations(info: &mut CdpPageTextInfo) {
    let mut annotations = Vec::new();
    if let Some(text) = &info.text {
        push_text_annotations(&mut annotations, "/page_text/text", text);
    }
    if annotations.is_empty() {
        info.perceived_text_notice = None;
        info.suspected_injection.clear();
    } else {
        info.perceived_text_notice = Some(PERCEIVED_TEXT_UNTRUSTED_NOTICE.to_owned());
        info.suspected_injection = annotations;
    }
}

fn chrome_active_element_info(
    active: &crate::chrome_debugger_bridge::ChromeDebuggerActiveElement,
) -> CdpActiveElementInfo {
    CdpActiveElementInfo {
        available: active.available,
        readback_source: active.readback_source.clone(),
        has_active_element: active.has_active_element,
        is_editable: active.is_editable,
        tag_name: active.tag_name.clone(),
        id_sha256: active.id.as_deref().and_then(non_empty_text_sha256),
        name_sha256: active.name.as_deref().and_then(non_empty_text_sha256),
        value_len: active.value.as_ref().map(|value| value.chars().count()),
        value_sha256: active.value.as_deref().and_then(non_empty_text_sha256),
        selected_text_sha256: active
            .selected_text
            .as_deref()
            .and_then(non_empty_text_sha256),
        error_code: active.error_code.clone(),
        error_detail_sha256: active
            .error_detail
            .as_deref()
            .and_then(non_empty_text_sha256),
    }
}

fn chrome_bridge_reload_response(
    session_id: &str,
    wait_timeout_ms: u64,
    reload: crate::chrome_debugger_bridge::ChromeBridgeReloadResult,
) -> CdpBridgeReloadResponse {
    CdpBridgeReloadResponse {
        session_id: session_id.to_owned(),
        required_foreground: false,
        wait_timeout_ms,
        before: chrome_bridge_host_readback(reload.before),
        command_ack: chrome_bridge_reload_ack_readback(reload.command_ack),
        after: chrome_bridge_host_readback(reload.after),
        reconnected: reload.reconnected,
        waited_ms: reload.waited_ms,
    }
}

fn chrome_bridge_host_readback(
    host: crate::chrome_debugger_bridge::ChromeBridgeHostSnapshot,
) -> CdpBridgeHostReadback {
    CdpBridgeHostReadback {
        host_id: host.host_id,
        origin: host.origin,
        extension_id: host.extension_id,
        extension_version: host.extension_version,
        extension_protocol_version: host.extension_protocol_version,
        extension_build_id: host.extension_build_id,
        extension_build_sha256: host.extension_build_sha256,
        extension_declared_build_sha256: host.extension_declared_build_sha256,
        extension_service_worker_sha256: host.extension_service_worker_sha256,
        extension_service_worker_sha256_status: host.extension_service_worker_sha256_status,
        extension_service_worker_sha256_source: host.extension_service_worker_sha256_source,
        extension_service_worker_byte_length: host.extension_service_worker_byte_length,
        extension_service_worker_sha256_error: host.extension_service_worker_sha256_error,
        expected_service_worker_sha256: host.expected_service_worker_sha256,
        expected_service_worker_path: host.expected_service_worker_path,
        extension_capabilities: host.extension_capabilities,
        extension_user_agent: host.extension_user_agent,
        extension_debugger_api_available: host.extension_debugger_api_available,
        pid: host.pid,
        parent_window: host.parent_window,
        transport: host.transport,
        registered_unix_ms: host.registered_unix_ms,
        last_seen_unix_ms: host.last_seen_unix_ms,
        last_disconnect_detail: host.last_disconnect_detail,
        last_detach_reason: host.last_detach_reason,
        extension_stale: host.extension_stale,
        extension_stale_reasons: host.extension_stale_reasons,
    }
}

fn chrome_bridge_reload_ack_readback(
    ack: crate::chrome_debugger_bridge::ChromeBridgeReloadCommandAck,
) -> CdpBridgeReloadAckReadback {
    CdpBridgeReloadAckReadback {
        ok: ack.ok,
        extension_id: ack.extension_id,
        version: ack.version,
        protocol_version: ack.protocol_version,
        build_id: ack.build_id,
        build_sha256: ack.build_sha256,
        declared_build_sha256: ack.declared_build_sha256,
        service_worker_sha256: ack.service_worker_sha256,
        service_worker_sha256_status: ack.service_worker_sha256_status,
        service_worker_sha256_source: ack.service_worker_sha256_source,
        service_worker_byte_length: ack.service_worker_byte_length,
        service_worker_sha256_error: ack.service_worker_sha256_error,
        debugger_api_available: ack.debugger_api_available,
        capabilities: ack.capabilities,
        host_id: ack.host_id,
        reload_requested_at_unix_ms: ack.reload_requested_at_unix_ms,
        reload_delay_ms: ack.reload_delay_ms,
    }
}

fn non_empty_text_sha256(value: &str) -> Option<String> {
    (!value.trim().is_empty()).then(|| sha256_hex(value.as_bytes()))
}

fn non_empty_string(value: Option<&str>) -> Option<String> {
    let value = value?.trim();
    (!value.is_empty()).then(|| value.to_owned())
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    hex_encode(&digest)
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len().saturating_mul(2));
    for byte in bytes {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    output
}

#[cfg(windows)]
const fn ocr_backend_name(backend: OcrBackend) -> &'static str {
    match backend {
        OcrBackend::Winrt => "winrt",
        OcrBackend::Crnn => "crnn",
        OcrBackend::Auto => "auto",
    }
}

fn input_backend_diagnostics(
    source: &str,
    policy: BackendResolutionPolicy,
) -> InputBackendDiagnostics {
    let vigem = vigem_capability();
    InputBackendDiagnostics {
        source: source.to_owned(),
        mouse_default: policy.mouse_auto_backend().as_str().to_owned(),
        keyboard_default: policy.keyboard_auto_backend().as_str().to_owned(),
        pad_default: policy.pad_auto_backend().as_str().to_owned(),
        release_all_default: policy.release_all_auto_backend().as_str().to_owned(),
        mouse: vec![
            available_backend(
                ResolvedBackend::Software,
                "software mouse input is available",
            ),
            unavailable_backend(
                ResolvedBackend::Vigem,
                error_codes::ACTION_BACKEND_UNAVAILABLE,
                "backend=vigem reason=ViGEm is a gamepad backend and cannot emit mouse input",
                false,
            ),
            hardware_unavailable("mouse"),
        ],
        keyboard: vec![
            available_backend(
                ResolvedBackend::Software,
                "software keyboard input is available",
            ),
            unavailable_backend(
                ResolvedBackend::Vigem,
                error_codes::ACTION_BACKEND_UNAVAILABLE,
                "backend=vigem reason=ViGEm is a gamepad backend and cannot emit keyboard input",
                false,
            ),
            hardware_unavailable("keyboard"),
        ],
        pad: vec![
            unavailable_backend(
                ResolvedBackend::Software,
                error_codes::ACTION_BACKEND_UNAVAILABLE,
                "backend=software reason=software backend does not emit virtual gamepad reports",
                false,
            ),
            vigem.clone(),
            hardware_unavailable("pad"),
        ],
        release_all: vec![
            available_backend(
                ResolvedBackend::Software,
                "software release_all is available for software-held input state",
            ),
            vigem,
            hardware_unavailable("release_all"),
        ],
    }
}

fn input_backend_diagnostics_from_error(
    source: &str,
    reason_code: impl Into<String>,
    reason: impl Into<String>,
) -> InputBackendDiagnostics {
    let capability = InputBackendCapability {
        backend: "unknown".to_owned(),
        available: false,
        reason_code: Some(reason_code.into()),
        reason: Some(reason.into()),
        host_boundary: false,
        transient: true,
    };
    InputBackendDiagnostics {
        source: source.to_owned(),
        mouse_default: "unknown".to_owned(),
        keyboard_default: "unknown".to_owned(),
        pad_default: "unknown".to_owned(),
        release_all_default: "unknown".to_owned(),
        mouse: vec![capability.clone()],
        keyboard: vec![capability.clone()],
        pad: vec![capability.clone()],
        release_all: vec![capability],
    }
}

fn available_backend(
    backend: ResolvedBackend,
    reason: impl Into<String>,
) -> InputBackendCapability {
    InputBackendCapability {
        backend: backend.as_str().to_owned(),
        available: true,
        reason_code: None,
        reason: Some(reason.into()),
        host_boundary: false,
        transient: false,
    }
}

fn unavailable_backend(
    backend: ResolvedBackend,
    reason_code: impl Into<String>,
    reason: impl Into<String>,
    transient: bool,
) -> InputBackendCapability {
    InputBackendCapability {
        backend: backend.as_str().to_owned(),
        available: false,
        reason_code: Some(reason_code.into()),
        reason: Some(reason.into()),
        host_boundary: true,
        transient,
    }
}

fn hardware_unavailable(class_name: &str) -> InputBackendCapability {
    unavailable_backend(
        ResolvedBackend::Hardware,
        error_codes::ACTION_BACKEND_UNAVAILABLE,
        format!(
            "backend=hardware reason=hardware backend removed; use backend=software for keyboard/mouse or backend=vigem for gamepad action_class={class_name}"
        ),
        false,
    )
}

fn vigem_capability() -> InputBackendCapability {
    match VigemBackend::new().ensure_ready() {
        Ok(()) => available_backend(
            ResolvedBackend::Vigem,
            "ViGEm virtual gamepad backend is available",
        ),
        Err(error) => unavailable_backend(
            ResolvedBackend::Vigem,
            error.code(),
            error.to_string(),
            false,
        ),
    }
}

#[cfg(windows)]
fn populate_profile_hud(
    input: &mut synapse_perception::ObservationInput,
    profile: &Profile,
    profile_dir: &Path,
) {
    for field in &profile.hud {
        input.hud.by_name.remove(&field.name);
        input.hud.errors.remove(&field.name);
        match extract_profile_hud_field(field, input.foreground.window_bounds, profile_dir) {
            Ok(reading) => {
                input.hud.by_name.insert(field.name.clone(), reading);
            }
            Err(error) => {
                record_hud_error(&mut input.hud, &field.name, error.code(), error.to_string());
            }
        }
    }
}

#[cfg(not(windows))]
fn populate_profile_hud(
    input: &mut synapse_perception::ObservationInput,
    profile: &Profile,
    _profile_dir: &std::path::Path,
) {
    for field in &profile.hud {
        input.hud.by_name.remove(&field.name);
        input.hud.errors.remove(&field.name);
        record_hud_error(
            &mut input.hud,
            &field.name,
            error_codes::HUD_EXTRACTION_FAILED,
            "profile HUD extraction requires Windows screen capture",
        );
    }
}

#[cfg(windows)]
fn extract_profile_hud_field(
    field: &HudFieldSpec,
    window_bounds: Rect,
    profile_dir: &Path,
) -> PerceptionResult<HudReading> {
    let screen_region = resolve_hud_region_rect(&field.region, window_bounds)?;
    let region_image = capture_region_gray(screen_region)?;
    match &field.extractor {
        HudExtractor::ColorRatio {
            sample_points: _,
            mapping,
        } => color_ratio_reading(field, screen_region, &region_image, mapping),
        HudExtractor::TemplateMatch { templates } => {
            let loaded_templates = load_templates(&field.name, templates, profile_dir)?;
            let provider = SystemOcrProvider;
            extract_field(&FieldExtractionRequest {
                field,
                screen_region,
                region_image: &region_image,
                templates: &loaded_templates,
                ocr_provider: &provider,
                stale_ms: 0,
            })
            .map(|extraction| extraction.reading)
        }
        HudExtractor::WinrtOcr | HudExtractor::Crnn { .. } => {
            let provider = HudTextProvider;
            extract_field(&FieldExtractionRequest {
                field,
                screen_region,
                region_image: &region_image,
                templates: &[],
                ocr_provider: &provider,
                stale_ms: 0,
            })
            .map(|extraction| extraction.reading)
        }
    }
}

#[cfg(windows)]
struct HudTextProvider;

#[cfg(windows)]
impl OcrProvider for HudTextProvider {
    fn read_text(&self, region: Rect) -> PerceptionResult<Vec<TextRegion>> {
        if let Some(text_region) = bounded_uia_text_region(region) {
            return Ok(vec![text_region]);
        }
        SystemOcrProvider.read_text(region)
    }
}

#[cfg(windows)]
fn bounded_uia_text_region(region: Rect) -> Option<TextRegion> {
    let point = region_center(region)?;
    let element = synapse_a11y::element_node_from_point(point).ok()?;
    let name = element.name.trim();
    if name.is_empty() {
        return None;
    }
    let bbox = element.bbox;
    if !uia_text_bbox_is_bound_to_hud_region(region, bbox) {
        return None;
    }
    Some(TextRegion {
        text: name.to_owned(),
        bbox,
        confidence: 1.0,
        confidence_source: synapse_perception::TextRegionConfidenceSource::Uia,
    })
}

#[cfg(windows)]
const fn region_center(region: Rect) -> Option<Point> {
    if region.w <= 0 || region.h <= 0 {
        return None;
    }
    Some(Point {
        x: region.x.saturating_add(region.w / 2),
        y: region.y.saturating_add(region.h / 2),
    })
}

#[cfg(windows)]
fn uia_text_bbox_is_bound_to_hud_region(region: Rect, bbox: Rect) -> bool {
    if region.w <= 0 || region.h <= 0 || bbox.w <= 0 || bbox.h <= 0 {
        return false;
    }
    let Some(region_area) = rect_area(region) else {
        return false;
    };
    let Some(bbox_area) = rect_area(bbox) else {
        return false;
    };
    bbox_area <= region_area.saturating_mul(4) && rects_intersect(region, bbox)
}

#[cfg(windows)]
fn rect_area(rect: Rect) -> Option<i64> {
    i64::from(rect.w).checked_mul(i64::from(rect.h))
}

#[cfg(windows)]
const fn rects_intersect(a: Rect, b: Rect) -> bool {
    let a_right = a.x.saturating_add(a.w);
    let a_bottom = a.y.saturating_add(a.h);
    let b_right = b.x.saturating_add(b.w);
    let b_bottom = b.y.saturating_add(b.h);
    a.x < b_right && a_right > b.x && a.y < b_bottom && a_bottom > b.y
}

#[cfg(windows)]
fn capture_region_gray(region: Rect) -> PerceptionResult<GrayImage> {
    let captured = synapse_capture::screen_region_to_bgra_bitmap(region).map_err(|error| {
        hud_error(format!(
            "HUD screen capture failed for region {region:?}: {error}"
        ))
    })?;
    bgra_to_gray(captured.width, captured.height, &captured.bytes)
}

#[cfg(windows)]
fn bgra_to_gray(width: u32, height: u32, bytes: &[u8]) -> PerceptionResult<GrayImage> {
    let expected_len = u64::from(width)
        .checked_mul(u64::from(height))
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| hud_error("HUD BGRA dimensions overflow"))?;
    let actual_len = u64::try_from(bytes.len())
        .map_err(|_err| hud_error("HUD BGRA byte length does not fit u64"))?;
    if actual_len < expected_len {
        return Err(hud_error(format!(
            "HUD BGRA buffer too short: expected at least {expected_len} bytes, got {actual_len}"
        )));
    }

    let mut image = GrayImage::new(width, height);
    for y in 0..height {
        for x in 0..width {
            let idx = usize::try_from((u64::from(y) * u64::from(width) + u64::from(x)) * 4)
                .map_err(|_err| hud_error("HUD BGRA pixel offset does not fit usize"))?;
            image.put_pixel(
                x,
                y,
                Luma([bgra_luma(bytes[idx], bytes[idx + 1], bytes[idx + 2])]),
            );
        }
    }
    Ok(image)
}

#[cfg(windows)]
fn color_ratio_reading(
    field: &HudFieldSpec,
    screen_region: Rect,
    region_image: &GrayImage,
    mapping: &str,
) -> PerceptionResult<HudReading> {
    if mapping != "luma_stddev_0_1" {
        return Err(hud_error(format!(
            "unsupported color_ratio mapping {mapping:?} for HUD field {:?}",
            field.name
        )));
    }
    let score = gray_luma_stddev_0_1(region_image);
    let raw_text = format!("{score:.6}");
    let parsed = parse_hud_text(&field.parser, &raw_text)?;
    Ok(HudReading {
        raw_text: format!(
            "{raw_text} region={}x{}@{},{}",
            screen_region.w, screen_region.h, screen_region.x, screen_region.y
        ),
        parsed,
        confidence: score,
        stale_ms: 0,
    })
}

#[cfg(windows)]
fn load_templates(
    field_name: &str,
    paths: &[String],
    profile_dir: &Path,
) -> PerceptionResult<Vec<HudTemplate>> {
    paths
        .iter()
        .enumerate()
        .map(|(index, path)| {
            let label = template_label(path, index);
            let value = template_value(field_name, path, index)?;
            let resolved = resolve_template_path(path, profile_dir);
            HudTemplate::load(label, value, resolved)
        })
        .collect()
}

#[cfg(windows)]
fn resolve_template_path(path: &str, profile_dir: &Path) -> PathBuf {
    let raw = Path::new(path);
    if raw.is_absolute() {
        return raw.to_path_buf();
    }

    let mut candidates = vec![PathBuf::from(path), profile_dir.join(path)];
    candidates.push(profile_dir.join("assets").join(path));
    if let Some(parent) = profile_dir.parent() {
        candidates.push(parent.join(path));
    }

    candidates
        .iter()
        .find(|candidate| candidate.exists())
        .cloned()
        .unwrap_or_else(|| profile_dir.join(path))
}

#[cfg(windows)]
fn template_label(path: &str, index: usize) -> String {
    Path::new(path)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .filter(|stem| !stem.trim().is_empty())
        .map_or_else(|| format!("template_{index}"), str::to_owned)
}

#[cfg(windows)]
fn template_value(field_name: &str, path: &str, index: usize) -> PerceptionResult<u32> {
    let lower_field = field_name.to_ascii_lowercase();
    let lower = path.to_ascii_lowercase();
    if lower_field.contains("hunger") {
        if lower.contains("full") || lower.contains("half") {
            return Ok(1);
        }
        if lower.contains("empty") {
            return Ok(0);
        }
    }
    if lower.contains("full") {
        return Ok(2);
    }
    if lower.contains("half") {
        return Ok(1);
    }
    if lower.contains("empty") {
        return Ok(0);
    }
    match index {
        0 => Ok(2),
        1 => Ok(1),
        2 => Ok(0),
        _ => Err(hud_error(format!(
            "cannot infer HUD template value for path {path:?}"
        ))),
    }
}

fn attach_observation_hygiene_annotations(
    observation: &mut synapse_core::Observation,
) -> Result<(), ErrorData> {
    let mut annotations = Vec::new();
    push_text_annotations(
        &mut annotations,
        "/foreground/window_title",
        &observation.foreground.window_title,
    );
    if let Some(focused) = &observation.focused {
        push_text_annotations(&mut annotations, "/focused/name", &focused.name);
        if let Some(value) = &focused.value {
            push_text_annotations(&mut annotations, "/focused/value", value);
        }
        if let Some(selected_text) = &focused.selected_text {
            push_text_annotations(&mut annotations, "/focused/selected_text", selected_text);
        }
    }
    for (index, element) in observation.elements.iter().enumerate() {
        push_text_annotations(
            &mut annotations,
            format!("/elements/{index}/name"),
            &element.name,
        );
        if let Some(value) = &element.value {
            push_text_annotations(&mut annotations, format!("/elements/{index}/value"), value);
        }
    }
    for (name, reading) in &observation.hud.by_name {
        push_text_annotations(
            &mut annotations,
            format!("/hud/by_name/{}/raw_text", escape_json_pointer(name)),
            &reading.raw_text,
        );
    }
    if let Some(clipboard) = &observation.clipboard_summary
        && let Some(text_excerpt) = &clipboard.text_excerpt
    {
        push_text_annotations(
            &mut annotations,
            "/clipboard_summary/text_excerpt",
            text_excerpt,
        );
    }
    for (index, fs_event) in observation.fs_recent.iter().enumerate() {
        push_text_annotations(
            &mut annotations,
            format!("/fs_recent/{index}/path"),
            &fs_event.path,
        );
    }
    for (index, event) in observation.recent_events.iter().enumerate() {
        collect_value_annotations(
            &event.data_excerpt,
            &format!("/recent_events/{index}/data_excerpt"),
            &mut annotations,
        );
    }
    apply_annotations_to_observation(observation, annotations)?;
    Ok(())
}

fn attach_ocr_hygiene_annotations(result: &mut OcrResult) {
    let mut annotations = Vec::new();
    push_text_annotations(&mut annotations, "/full_text", &result.full_text);
    for (index, word) in result.words.iter().enumerate() {
        push_text_annotations(&mut annotations, format!("/words/{index}/text"), &word.text);
    }
    apply_annotations_to_ocr_result(result, annotations);
}

fn attach_find_hygiene_annotations(response: &mut FindResponse) {
    let mut annotations = Vec::new();
    for (index, result) in response.results.iter().enumerate() {
        if let Some(name) = &result.name {
            push_text_annotations(&mut annotations, format!("/results/{index}/name"), name);
        }
        if let Some(role) = &result.role {
            push_text_annotations(&mut annotations, format!("/results/{index}/role"), role);
        }
        if let Some(automation_id) = &result.automation_id {
            push_text_annotations(
                &mut annotations,
                format!("/results/{index}/automation_id"),
                automation_id,
            );
        }
        if let Some(class_label) = &result.class_label {
            push_text_annotations(
                &mut annotations,
                format!("/results/{index}/class_label"),
                class_label,
            );
        }
    }
    if annotations.is_empty() {
        response.perceived_text_notice = None;
        response.suspected_injection.clear();
    } else {
        response.perceived_text_notice = Some(PERCEIVED_TEXT_UNTRUSTED_NOTICE.to_owned());
        response.suspected_injection = annotations;
    }
}

fn collect_value_annotations(
    value: &Value,
    path: &str,
    annotations: &mut Vec<SuspectedInjectionAnnotation>,
) {
    match value {
        Value::String(text) => push_text_annotations(annotations, path, text),
        Value::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                collect_value_annotations(item, &format!("{path}/{index}"), annotations);
            }
        }
        Value::Object(map) => {
            for (key, item) in map {
                collect_value_annotations(
                    item,
                    &format!("{path}/{}", escape_json_pointer(key)),
                    annotations,
                );
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

fn push_text_annotations(
    annotations: &mut Vec<SuspectedInjectionAnnotation>,
    source_path: impl Into<String>,
    text: &str,
) {
    if text.trim().is_empty() {
        return;
    }
    annotations.extend(crate::m3::hygiene::scan_perceived_text(source_path, text));
}

fn apply_annotations_to_observation(
    observation: &mut synapse_core::Observation,
    annotations: Vec<SuspectedInjectionAnnotation>,
) -> Result<(), ErrorData> {
    if annotations.is_empty() {
        observation.perceived_text_notice = None;
        observation.suspected_injection.clear();
    } else {
        observation.perceived_text_notice = Some(PERCEIVED_TEXT_UNTRUSTED_NOTICE.to_owned());
        observation.suspected_injection = annotations;
    }
    refresh_observation_size_fields(observation)
}

fn apply_annotations_to_ocr_result(
    result: &mut OcrResult,
    annotations: Vec<SuspectedInjectionAnnotation>,
) {
    if annotations.is_empty() {
        result.perceived_text_notice = None;
        result.suspected_injection.clear();
    } else {
        result.perceived_text_notice = Some(PERCEIVED_TEXT_UNTRUSTED_NOTICE.to_owned());
        result.suspected_injection = annotations;
    }
}

fn refresh_observation_size_fields(
    observation: &mut synapse_core::Observation,
) -> Result<(), ErrorData> {
    for _ in 0..2 {
        let size_bytes = u32::try_from(
            serde_json::to_vec(observation)
                .map_err(|error| mcp_error(error_codes::OBSERVE_INTERNAL, error.to_string()))?
                .len(),
        )
        .unwrap_or(u32::MAX);
        observation.diagnostics.size_bytes = size_bytes;
        observation.diagnostics.size_estimate_tokens = size_bytes.div_ceil(4);
    }
    Ok(())
}

fn escape_json_pointer(segment: &str) -> String {
    segment.replace('~', "~0").replace('/', "~1")
}

#[cfg(all(test, windows))]
mod tests;

#[cfg(windows)]
fn gray_luma_stddev_0_1(region_image: &GrayImage) -> f32 {
    let mut count = 0.0_f32;
    let mut sum = 0.0_f32;
    let mut sum_sq = 0.0_f32;
    for pixel in region_image.pixels() {
        let luma = f32::from(pixel.0[0]);
        count += 1.0;
        sum += luma;
        sum_sq = luma.mul_add(luma, sum_sq);
    }
    if count <= 0.0 {
        return 0.0;
    }
    let mean = sum / count;
    let variance = mean.mul_add(-mean, sum_sq / count).max(0.0);
    (variance.sqrt() / 128.0).clamp(0.0, 1.0)
}

#[cfg(windows)]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn bgra_luma(b: u8, g: u8, r: u8) -> u8 {
    let luma = 0.0722_f32.mul_add(
        f32::from(b),
        0.7152_f32.mul_add(f32::from(g), 0.2126_f32 * f32::from(r)),
    );
    luma.round().clamp(0.0, 255.0) as u8
}

#[cfg(windows)]
fn hud_error(detail: impl Into<String>) -> PerceptionError {
    PerceptionError::HudExtractionFailed {
        detail: detail.into(),
    }
}

fn record_hud_error(
    hud: &mut HudReadings,
    field_name: &str,
    code: &'static str,
    detail: impl Into<String>,
) {
    hud.errors.insert(
        field_name.to_owned(),
        HudFieldError {
            code: code.to_owned(),
            detail: detail.into(),
        },
    );
}
