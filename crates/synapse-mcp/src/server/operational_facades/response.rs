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
        anchors: None,
        temporal_panels: None,
        corpus_histogram: None,
        temporal_rerank: None,
        temporal_backfill: None,
        search_rebuild: None,
        find_similar: None,
        retire_orphan_slot_cfs: None,
        backup: None,
        restore_verify: None,
        intelligence: None,
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
        grounding_gap: None,
        blind_spot: None,
        drift: None,
        vault_verify: None,
        kernel: None,
        kernel_rebuild: None,
        guard_calibrate: None,
        guard_verify: None,
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
        host_transition: None,
    };
    fill(&mut response);
    response
}
