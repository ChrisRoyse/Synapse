use super::super::{
    reality::{RealityAuditParams, RealityBaselineParams},
    verification::VerificationAuditParams,
};
use super::{assist::*, reality::*, routine::*, verification::*};
use crate::m3::{
    routines::{RoutineListParams, RoutineMineParams},
    suggestions::SuggestionListParams,
};

fn empty_routine_params(operation: RoutineOperation) -> RoutineParams {
    RoutineParams {
        operation,
        mine: None,
        list: None,
        inspect: None,
        update: None,
        feedback: None,
        label: None,
        automate: None,
        armed_tick: None,
    }
}

fn empty_assist_params(operation: AssistOperation) -> AssistParams {
    AssistParams {
        operation,
        intent: None,
        detect: None,
        suggestion_tick: None,
        suggestion_list: None,
        suggestion_accept: None,
    }
}

fn empty_reality_params(operation: RealityOperation) -> RealityParams {
    RealityParams {
        operation,
        baseline: None,
        delta: None,
        audit: None,
    }
}

fn empty_verification_params(operation: VerificationOperation) -> VerificationParams {
    VerificationParams {
        operation,
        inbox: None,
        poll: None,
        audit: None,
        bind: None,
        sources: None,
    }
}

#[test]
fn routine_facade_params_require_exact_matching_spec() {
    let missing = validate_routine_facade_params(&empty_routine_params(RoutineOperation::Inspect))
        .expect_err("missing inspect spec should fail");
    assert!(
        missing
            .message
            .to_string()
            .contains("requires a matching inspect spec"),
        "{missing:?}"
    );

    let mut extra = empty_routine_params(RoutineOperation::List);
    extra.list = Some(RoutineListParams::default());
    extra.mine = Some(RoutineMineParams::default());
    let error =
        validate_routine_facade_params(&extra).expect_err("multiple routine specs should fail");
    assert!(
        error
            .message
            .to_string()
            .contains("received invalid operation specs"),
        "{error:?}"
    );
}

#[test]
fn assist_facade_params_require_exact_matching_spec() {
    let missing =
        validate_assist_facade_params(&empty_assist_params(AssistOperation::SuggestionList))
            .expect_err("missing suggestion_list spec should fail");
    assert!(
        missing
            .message
            .to_string()
            .contains("requires a matching suggestion_list spec"),
        "{missing:?}"
    );

    let mut valid = empty_assist_params(AssistOperation::SuggestionList);
    valid.suggestion_list = Some(SuggestionListParams::default());
    validate_assist_facade_params(&valid).expect("matching suggestion_list spec should pass");
}

#[test]
fn reality_facade_params_require_exact_matching_spec() {
    let missing = validate_reality_facade_params(&empty_reality_params(RealityOperation::Audit))
        .expect_err("missing audit spec should fail");
    assert!(
        missing
            .message
            .to_string()
            .contains("requires a matching audit spec"),
        "{missing:?}"
    );

    let mut extra = empty_reality_params(RealityOperation::Baseline);
    extra.baseline = Some(RealityBaselineParams {
        profile_id: None,
        epoch_id: None,
        force_new_epoch: false,
        include: Vec::new(),
        depth: 1,
        max_elements: 1,
    });
    extra.audit = Some(RealityAuditParams {
        profile_id: None,
        epoch_id: None,
        assumption_hash: None,
        include: Vec::new(),
        depth: 1,
        max_elements: 1,
    });
    let error =
        validate_reality_facade_params(&extra).expect_err("multiple reality specs should fail");
    assert!(
        error
            .message
            .to_string()
            .contains("received invalid operation specs"),
        "{error:?}"
    );
}

#[test]
fn verification_facade_params_require_exact_matching_spec() {
    let missing = validate_verification_facade_params(&empty_verification_params(
        VerificationOperation::Sources,
    ))
    .expect_err("missing sources spec should fail");
    assert!(
        missing
            .message
            .to_string()
            .contains("requires a matching sources spec"),
        "{missing:?}"
    );

    let mut valid = empty_verification_params(VerificationOperation::Audit);
    valid.audit = Some(VerificationAuditParams { max: None });
    validate_verification_facade_params(&valid).expect("matching audit spec should pass");
}
