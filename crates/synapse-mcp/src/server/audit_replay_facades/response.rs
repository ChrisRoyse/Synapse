use super::{
    AUDIT_SOT, REPLAY_SOT,
    types::{AuditOperation, AuditResponse, ReplayOperation, ReplayResponse},
};
pub(super) fn audit_response(
    operation: AuditOperation,
    readback: String,
    fill: impl FnOnce(&mut AuditResponse),
) -> AuditResponse {
    let mut response = AuditResponse {
        operation,
        source_of_truth: AUDIT_SOT.to_owned(),
        readback_source_of_truth: readback,
        command_query: None,
        lifecycle_events: None,
        lifecycle_exits: None,
        profile_intelligence: None,
        export_bundle: None,
    };
    fill(&mut response);
    response
}

pub(super) fn replay_response(
    operation: ReplayOperation,
    readback: String,
    fill: impl FnOnce(&mut ReplayResponse),
) -> ReplayResponse {
    let mut response = ReplayResponse {
        operation,
        source_of_truth: REPLAY_SOT.to_owned(),
        readback_source_of_truth: readback,
        record: None,
        demo_status: None,
        demo_start: None,
        demo_stop: None,
        artifact_readback: None,
    };
    fill(&mut response);
    response
}
