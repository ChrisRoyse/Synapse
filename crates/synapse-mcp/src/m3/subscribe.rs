use chrono::{DateTime, Utc};
use rmcp::{ErrorData, schemars::JsonSchema};
use serde::{Deserialize, Serialize};
use synapse_core::{EventFilter, error_codes};

use crate::{http::sse::SseState, m1::mcp_error};

use super::{
    M3ToolStub, a11y_events,
    permissions::{Permission, RequiredPermissions, required},
};

const DEFAULT_BUFFER_SIZE: u32 = 4096;

const fn default_kinds() -> Vec<String> {
    Vec::new()
}

const fn default_snapshot_first() -> bool {
    false
}

const fn default_buffer_size() -> u32 {
    DEFAULT_BUFFER_SIZE
}

const fn default_filter() -> Option<EventFilter> {
    None
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SubscribeParams {
    #[serde(default = "default_kinds")]
    #[schemars(default = "default_kinds")]
    pub kinds: Vec<String>,
    #[serde(default = "default_filter")]
    #[schemars(default = "default_filter")]
    pub filter: Option<EventFilter>,
    #[serde(default = "default_snapshot_first")]
    #[schemars(default = "default_snapshot_first")]
    pub snapshot_first: bool,
    /// Fixed SSE ring capacity. The only accepted value is 4096: the buffer is
    /// never actually resized (this field is not forwarded to the subscription),
    /// and `subscribe_to_events` rejects anything else. The schema therefore
    /// pins it rather than advertising the full u32 range it does not honour --
    /// a client that trusted the open range got TOOL_PARAMS_INVALID.
    #[serde(default = "default_buffer_size")]
    #[schemars(default = "default_buffer_size")]
    #[schemars(range(min = 4096, max = 4096))]
    pub buffer_size: u32,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SubscribeResponse {
    pub subscription_id: String,
    pub started_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SubscribeCancelParams {
    pub subscription_id: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SubscribeCancelReason {
    Ok,
    #[expect(
        dead_code,
        reason = "schema advertises not_found while runtime returns SUBSCRIPTION_NOT_FOUND errors"
    )]
    NotFound,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SubscribeCancelResponse {
    pub cancelled: bool,
    pub reason: SubscribeCancelReason,
}

#[must_use]
pub const fn subscribe() -> M3ToolStub {
    M3ToolStub::new("subscribe")
}

#[must_use]
pub const fn subscribe_cancel() -> M3ToolStub {
    M3ToolStub::new("subscribe_cancel")
}

#[must_use]
pub fn required_permissions(_params: &SubscribeParams) -> RequiredPermissions {
    required([Permission::ReadEvents])
}

#[must_use]
pub fn required_permissions_cancel(_params: &SubscribeCancelParams) -> RequiredPermissions {
    required([Permission::ReadEvents])
}

#[must_use]
pub fn requires_a11y_event_bridge(params: &SubscribeParams) -> bool {
    a11y_events::kinds_require_a11y_bridge(&params.kinds)
        || params
            .filter
            .as_ref()
            .is_some_and(a11y_events::event_filter_requires_a11y_bridge)
}

pub fn subscribe_to_events(
    sse_state: &SseState,
    params: &SubscribeParams,
    owner_session_id: Option<String>,
) -> Result<SubscribeResponse, ErrorData> {
    if params.buffer_size != DEFAULT_BUFFER_SIZE {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            format!(
                "subscribe buffer_size must be {DEFAULT_BUFFER_SIZE}; got {}",
                params.buffer_size
            ),
        ));
    }
    let filter = params.filter.clone().unwrap_or(EventFilter::All);
    let subscription_id = sse_state
        .subscribe(
            filter,
            params.kinds.clone(),
            params.snapshot_first,
            owner_session_id,
        )
        .map_err(|error| mcp_error(error.code(), error.message()))?;
    Ok(SubscribeResponse {
        subscription_id,
        started_at: Utc::now(),
    })
}

pub fn cancel_subscription(
    sse_state: &SseState,
    params: &SubscribeCancelParams,
) -> Result<SubscribeCancelResponse, ErrorData> {
    let subscription_id = params.subscription_id.trim();
    if subscription_id.is_empty() {
        return Err(mcp_error(
            error_codes::TOOL_PARAMS_INVALID,
            "subscribe_cancel subscription_id must not be empty",
        ));
    }
    sse_state
        .cancel(subscription_id)
        .map_err(|error| mcp_error(error.code(), error.message(subscription_id)))?;
    Ok(SubscribeCancelResponse {
        cancelled: true,
        reason: SubscribeCancelReason::Ok,
    })
}
