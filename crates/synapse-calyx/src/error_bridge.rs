use crate::SynapseCalyxError;

pub const SYNAPSE_CALYX_UNMAPPED_ERROR: &str = "SYNAPSE_CALYX_UNMAPPED_ERROR";
pub const SYNAPSE_CALYX_ERROR_MAPPING_INVALID: &str = "SYNAPSE_CALYX_ERROR_MAPPING_INVALID";
pub const SYNAPSE_CALYX_BACKPRESSURE: &str = "SYNAPSE_CALYX_BACKPRESSURE";

const CALYX_CORE_ERROR_CODE_COUNT: usize = 43;
const _: [(); CALYX_CORE_ERROR_CODE_COUNT] = [(); calyx_core::CALYX_ERROR_CODES.len()];

const CALYX_ERROR_BRIDGE: &[(&str, &str)] = &[
    (
        "CALYX_LENS_FROZEN_VIOLATION",
        "SYNAPSE_CALYX_LENS_FROZEN_VIOLATION",
    ),
    ("CALYX_LENS_DIM_MISMATCH", "SYNAPSE_CALYX_LENS_DIM_MISMATCH"),
    (
        "CALYX_LENS_NUMERICAL_INVARIANT",
        "SYNAPSE_CALYX_LENS_NUMERICAL_INVARIANT",
    ),
    ("CALYX_LENS_UNREACHABLE", "SYNAPSE_CALYX_LENS_UNREACHABLE"),
    (
        "CALYX_REGISTRY_DUPLICATE",
        "SYNAPSE_CALYX_REGISTRY_DUPLICATE",
    ),
    (
        "CALYX_REGISTRY_UNAVAILABLE",
        "SYNAPSE_CALYX_REGISTRY_UNAVAILABLE",
    ),
    (
        "CALYX_ASSAY_INSUFFICIENT_SAMPLES",
        "SYNAPSE_CALYX_ASSAY_INSUFFICIENT_SAMPLES",
    ),
    (
        "CALYX_FUSION_TUNING_INVALID",
        "SYNAPSE_CALYX_FUSION_TUNING_INVALID",
    ),
    ("CALYX_ASSAY_LOW_SIGNAL", "SYNAPSE_CALYX_ASSAY_LOW_SIGNAL"),
    ("CALYX_ASSAY_REDUNDANT", "SYNAPSE_CALYX_ASSAY_REDUNDANT"),
    (
        "CALYX_ASSAY_DEGENERATE_INPUT",
        "SYNAPSE_CALYX_ASSAY_DEGENERATE_INPUT",
    ),
    (
        "CALYX_ASSAY_OCCURRENCES_NOT_MONOTONIC",
        "SYNAPSE_CALYX_ASSAY_OCCURRENCES_NOT_MONOTONIC",
    ),
    ("CALYX_KERNEL_UNGROUNDED", "SYNAPSE_CALYX_KERNEL_UNGROUNDED"),
    ("CALYX_GUARD_PROVISIONAL", "SYNAPSE_CALYX_GUARD_PROVISIONAL"),
    ("CALYX_GUARD_OOD", "SYNAPSE_CALYX_GUARD_OOD"),
    (
        "CALYX_FORGE_NUMERICAL_INVARIANT",
        "SYNAPSE_CALYX_FORGE_NUMERICAL_INVARIANT",
    ),
    (
        "CALYX_FORGE_DEVICE_UNAVAILABLE",
        "SYNAPSE_CALYX_FORGE_DEVICE_UNAVAILABLE",
    ),
    (
        "CALYX_ASTER_CORRUPT_SHARD",
        "SYNAPSE_CALYX_ASTER_CORRUPT_SHARD",
    ),
    ("CALYX_ASTER_TORN_WAL", "SYNAPSE_CALYX_ASTER_TORN_WAL"),
    (
        "CALYX_LEDGER_CHAIN_BROKEN",
        "SYNAPSE_CALYX_LEDGER_CHAIN_BROKEN",
    ),
    ("CALYX_LEDGER_CORRUPT", "SYNAPSE_CALYX_LEDGER_CORRUPT"),
    (
        "CALYX_LEDGER_APPEND_ONLY_VIOLATION",
        "SYNAPSE_CALYX_LEDGER_APPEND_ONLY_VIOLATION",
    ),
    (
        "CALYX_LEDGER_SECRET_IN_PAYLOAD",
        "SYNAPSE_CALYX_LEDGER_SECRET_IN_PAYLOAD",
    ),
    (
        "CALYX_LEDGER_ACTOR_TOO_LONG",
        "SYNAPSE_CALYX_LEDGER_ACTOR_TOO_LONG",
    ),
    (
        "CALYX_LEDGER_GROUP_COMMIT_FAILED",
        "SYNAPSE_CALYX_LEDGER_GROUP_COMMIT_FAILED",
    ),
    (
        "CALYX_REPRODUCE_NONDETERMINISTIC",
        "SYNAPSE_CALYX_REPRODUCE_NONDETERMINISTIC",
    ),
    (
        "CALYX_REPRODUCE_DRIFT_EXCEEDED",
        "SYNAPSE_CALYX_REPRODUCE_DRIFT_EXCEEDED",
    ),
    (
        "CALYX_VAULT_ACCESS_DENIED",
        "SYNAPSE_CALYX_VAULT_ACCESS_DENIED",
    ),
    (
        "CALYX_ERASE_ALREADY_TOMBSTONED",
        "SYNAPSE_CALYX_ERASE_ALREADY_TOMBSTONED",
    ),
    ("CALYX_STALE_DERIVED", "SYNAPSE_CALYX_STALE_DERIVED"),
    (
        "CALYX_ORACLE_INSUFFICIENT",
        "SYNAPSE_CALYX_ORACLE_INSUFFICIENT",
    ),
    ("CALYX_FORGE_VRAM_BUDGET", "SYNAPSE_CALYX_FORGE_VRAM_BUDGET"),
    ("CALYX_BACKPRESSURE", SYNAPSE_CALYX_BACKPRESSURE),
    ("CALYX_DISK_PRESSURE", "SYNAPSE_CALYX_DISK_PRESSURE"),
    (
        "CALYX_QUANT_INTELLIGENCE_LOSS",
        "SYNAPSE_CALYX_QUANT_INTELLIGENCE_LOSS",
    ),
    (
        "CALYX_READER_LEASE_EXPIRED",
        "SYNAPSE_CALYX_READER_LEASE_EXPIRED",
    ),
    ("CALYX_DATASET_NOT_FOUND", "SYNAPSE_CALYX_DATASET_NOT_FOUND"),
    (
        "CALYX_DATASET_CHECKSUM_MISMATCH",
        "SYNAPSE_CALYX_DATASET_CHECKSUM_MISMATCH",
    ),
    (
        "CALYX_DATASET_ROWCOUNT_MISMATCH",
        "SYNAPSE_CALYX_DATASET_ROWCOUNT_MISMATCH",
    ),
    (
        "CALYX_DATASET_MANIFEST_INVALID",
        "SYNAPSE_CALYX_DATASET_MANIFEST_INVALID",
    ),
    (
        "CALYX_DATASET_SCHEMA_MISMATCH",
        "SYNAPSE_CALYX_DATASET_SCHEMA_MISMATCH",
    ),
    (
        "CALYX_ASTER_VAULT_SCHEMA_AHEAD",
        "SYNAPSE_CALYX_ASTER_VAULT_SCHEMA_AHEAD",
    ),
    (
        "CALYX_ASTER_PANEL_SLOT_SET_IMMUTABLE",
        "SYNAPSE_CALYX_ASTER_PANEL_SLOT_SET_IMMUTABLE",
    ),
];

pub fn map_calyx_error_code(calyx_code: &str) -> Option<&'static str> {
    CALYX_ERROR_BRIDGE
        .iter()
        .find_map(|(candidate, mapped)| (*candidate == calyx_code).then_some(*mapped))
}

pub fn validate_calyx_error_bridge() -> Result<(), SynapseCalyxError> {
    if CALYX_ERROR_BRIDGE.len() != calyx_core::CALYX_ERROR_CODES.len() {
        return Err(invalid_bridge(format!(
            "Calyx PRD-18 catalog has {} codes but Synapse maps {} codes",
            calyx_core::CALYX_ERROR_CODES.len(),
            CALYX_ERROR_BRIDGE.len()
        )));
    }

    for calyx_code in calyx_core::CALYX_ERROR_CODES {
        let code = calyx_code.code();
        match map_calyx_error_code(code) {
            Some(mapped) if mapped.starts_with("SYNAPSE_CALYX_") => {}
            Some(mapped) => {
                return Err(invalid_bridge(format!(
                    "Calyx code {code} maps outside the Synapse Calyx error namespace: {mapped}"
                )));
            }
            None => {
                return Err(invalid_bridge(format!(
                    "Calyx code {code} has no Synapse error mapping"
                )));
            }
        }
    }

    for (idx, (calyx_code, synapse_code)) in CALYX_ERROR_BRIDGE.iter().enumerate() {
        if !calyx_code.starts_with("CALYX_") {
            return Err(invalid_bridge(format!(
                "mapping row {idx} has non-Calyx source code {calyx_code}"
            )));
        }
        if !synapse_code.starts_with("SYNAPSE_CALYX_") {
            return Err(invalid_bridge(format!(
                "mapping row {idx} has non-Synapse mapped code {synapse_code}"
            )));
        }
        for (later_calyx, later_synapse) in CALYX_ERROR_BRIDGE.iter().skip(idx + 1) {
            if later_calyx == calyx_code {
                return Err(invalid_bridge(format!(
                    "duplicate Calyx source mapping for {calyx_code}"
                )));
            }
            if later_synapse == synapse_code {
                return Err(invalid_bridge(format!(
                    "duplicate Synapse mapped code for {synapse_code}"
                )));
            }
        }
    }

    Ok(())
}

fn invalid_bridge(message: String) -> SynapseCalyxError {
    SynapseCalyxError::new(
        SYNAPSE_CALYX_ERROR_MAPPING_INVALID,
        message,
        "update crates/synapse-calyx/src/error_bridge.rs before exposing this Calyx error through Synapse",
    )
}
