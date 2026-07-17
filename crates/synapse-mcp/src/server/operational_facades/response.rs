use super::{
    HYGIENE_SOT, MODEL_SOT, SETUP_SOT, STORAGE_SOT,
    types::{
        HygieneOperation, HygieneResponse, ModelOperation, ModelResponse, SetupOperation,
        SetupResponse, StorageOperation, StorageResponse,
    },
};
pub(super) fn storage_response(
    operation: StorageOperation,
    readback: String,
    fill: impl FnOnce(&mut StorageResponse),
) -> StorageResponse {
    let mut response = StorageResponse {
        operation,
        source_of_truth: STORAGE_SOT.to_owned(),
        readback_source_of_truth: readback,
        inspect: None,
        summary: None,
        gc_once: None,
    };
    fill(&mut response);
    response
}

pub(super) fn model_response(
    operation: ModelOperation,
    readback: String,
    fill: impl FnOnce(&mut ModelResponse),
) -> ModelResponse {
    let mut response = ModelResponse {
        operation,
        source_of_truth: MODEL_SOT.to_owned(),
        readback_source_of_truth: readback,
        list: None,
        status: None,
        probe: None,
        register: None,
        update: None,
        remove: None,
    };
    fill(&mut response);
    response
}

pub(super) fn hygiene_response(
    operation: HygieneOperation,
    readback: String,
    fill: impl FnOnce(&mut HygieneResponse),
) -> HygieneResponse {
    let mut response = HygieneResponse {
        operation,
        source_of_truth: HYGIENE_SOT.to_owned(),
        readback_source_of_truth: readback,
        scan_text: None,
        scan_storage: None,
        flags: None,
        report: None,
    };
    fill(&mut response);
    response
}

pub(super) fn setup_response(
    operation: SetupOperation,
    readback: String,
    fill: impl FnOnce(&mut SetupResponse),
) -> SetupResponse {
    let mut response = SetupResponse {
        operation,
        source_of_truth: SETUP_SOT.to_owned(),
        readback_source_of_truth: readback,
        status: None,
        doctor: None,
    };
    fill(&mut response);
    response
}
