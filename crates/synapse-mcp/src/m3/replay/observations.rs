use rmcp::ErrorData;
use synapse_core::error_codes;
use synapse_perception::{ObservationAssembler, ObserveInclude};
use tokio::io::AsyncWrite;

use crate::m1::{SharedM1State, current_input, mcp_error};

use super::{ReplayRecordLine, ReplayTarget, serializer::write_json_line};

pub(super) async fn write_observation<W>(
    writer: &mut W,
    m1_state: &SharedM1State,
    assembler: &ObservationAssembler,
    include: ObserveInclude,
    target: ReplayTarget,
) -> Result<ObservationWrite, ErrorData>
where
    W: AsyncWrite + Unpin + Send,
{
    let input = {
        let state = m1_state.lock().map_err(|_err| {
            mcp_error(
                error_codes::OBSERVE_INTERNAL,
                "M1 service state lock poisoned",
            )
        })?;
        match current_input(&state, include.max_subtree_depth) {
            Ok(input) => input,
            Err(error) if is_no_perception_error(&error) => {
                return Ok(ObservationWrite::skipped());
            }
            Err(error) => return Err(error),
        }
    };
    let observation = match assembler
        .assemble(include, input)
        .map_err(|error| mcp_error(error.code(), error.to_string()))
    {
        Ok(observation) => observation,
        Err(error) if is_no_perception_error(&error) => return Ok(ObservationWrite::skipped()),
        Err(error) => return Err(error),
    };
    match target {
        ReplayTarget::Observations => write_json_line(writer, &observation).await?,
        ReplayTarget::Both => {
            write_json_line(
                writer,
                &ReplayRecordLine::Observation {
                    record: &observation,
                },
            )
            .await?;
        }
        ReplayTarget::Events => {}
    }
    Ok(ObservationWrite::written(target))
}

#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub(super) struct ObservationWrite {
    pub(super) records_written: u64,
    pub(super) observations_written: u64,
    pub(super) observations_skipped: u64,
}

impl ObservationWrite {
    const fn skipped() -> Self {
        Self {
            records_written: 0,
            observations_written: 0,
            observations_skipped: 1,
        }
    }

    const fn written(target: ReplayTarget) -> Self {
        Self {
            records_written: if target.includes_observations() { 1 } else { 0 },
            observations_written: 1,
            observations_skipped: 0,
        }
    }
}

fn is_no_perception_error(error: &ErrorData) -> bool {
    error
        .data
        .as_ref()
        .and_then(|data| data.get("code"))
        .and_then(serde_json::Value::as_str)
        == Some(error_codes::OBSERVE_NO_PERCEPTION_AVAILABLE)
}
