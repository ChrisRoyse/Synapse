use rmcp::{ErrorData, schemars::JsonSchema};
use schemars::{Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Serialize};
use synapse_core::{Event, Observation, error_codes};

use crate::m1::mcp_error;

use super::{
    M3ToolStub,
    permissions::{Permission, RequiredPermissions, required},
};

mod events;
mod observations;
mod record;
mod serializer;
#[cfg(test)]
mod tests;

pub use self::record::record_replay;

const DEFAULT_TARGET: &str = "observations";
const DEFAULT_FORMAT: &str = "jsonl";
const OBSERVATION_SAMPLE_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);
const EVENT_DRAIN_INTERVAL: std::time::Duration = std::time::Duration::from_millis(20);

fn default_target() -> String {
    DEFAULT_TARGET.to_owned()
}

fn default_format() -> String {
    DEFAULT_FORMAT.to_owned()
}

fn replay_target_schema(_: &mut SchemaGenerator) -> Schema {
    json_schema!({
        "type": "string",
        "enum": ["observations", "events", "both"],
        "default": DEFAULT_TARGET
    })
}

fn replay_format_schema(_: &mut SchemaGenerator) -> Schema {
    json_schema!({
        "type": "string",
        "enum": [DEFAULT_FORMAT],
        "default": DEFAULT_FORMAT
    })
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReplayRecordParams {
    #[serde(default = "default_target")]
    #[schemars(schema_with = "replay_target_schema")]
    pub target: String,
    #[serde(default = "default_format")]
    #[schemars(schema_with = "replay_format_schema")]
    pub format: String,
    #[schemars(range(min = 0))]
    pub duration_ms: u32,
    #[serde(default)]
    pub path: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReplayRecordResponse {
    pub path: String,
    pub records_written: u64,
    pub observations_skipped: u64,
    pub bytes: u64,
}

#[must_use]
pub const fn replay_record() -> M3ToolStub {
    M3ToolStub::new("replay_record")
}

#[must_use]
pub fn required_permissions(_params: &ReplayRecordParams) -> RequiredPermissions {
    required([Permission::WriteReplay])
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum ReplayTarget {
    Observations,
    Events,
    Both,
}

impl ReplayTarget {
    fn parse(value: &str) -> Result<Self, ErrorData> {
        match value.trim() {
            "observations" => Ok(Self::Observations),
            "events" => Ok(Self::Events),
            "both" => Ok(Self::Both),
            other => Err(mcp_error(
                error_codes::REPLAY_TARGET_INVALID,
                format!(
                    "replay_record target must be one of observations, events, or both; got {other:?}"
                ),
            )),
        }
    }

    const fn includes_observations(self) -> bool {
        matches!(self, Self::Observations | Self::Both)
    }

    const fn includes_events(self) -> bool {
        matches!(self, Self::Events | Self::Both)
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum ReplayFormat {
    Jsonl,
}

impl ReplayFormat {
    fn parse(value: &str) -> Result<Self, ErrorData> {
        match value.trim() {
            DEFAULT_FORMAT => Ok(Self::Jsonl),
            other => Err(mcp_error(
                error_codes::REPLAY_FORMAT_INVALID,
                format!("replay_record format must be jsonl; got {other:?}"),
            )),
        }
    }
}

#[derive(Serialize)]
#[serde(tag = "target", rename_all = "snake_case")]
enum ReplayRecordLine<'a> {
    Observation { record: &'a Observation },
    Event { record: &'a Event },
}
