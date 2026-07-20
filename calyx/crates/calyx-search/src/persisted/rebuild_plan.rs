use std::collections::{BTreeMap, BTreeSet};
use std::env;

use calyx_core::{CxId, SlotId};
use calyx_sextant::index::DiskAnnBuildBackend;

use crate::error::{CliError, CliResult};
use crate::persisted::SearchIndexManifest;

pub(super) const DISKANN_BUILD_BACKEND_ENV: &str = "CALYX_SEARCH_DISKANN_BUILD_BACKEND";
const REQUIRE_CUVS_ENV: &str = "CALYX_SEARCH_REQUIRE_CUVS_CAGRA";
const ALLOW_CPU_REFERENCE_ENV: &str = "CALYX_SEARCH_ALLOW_CPU_DISKANN_REBUILD";
const CUVS_UNAVAILABLE_CODE: &str = "CALYX_SEARCH_CUVS_CAGRA_UNAVAILABLE";
const CPU_REFERENCE_CODE: &str = "CALYX_SEARCH_CPU_REFERENCE_OVERRIDE_REQUIRED";
const CUVS_REMEDIATION: &str = "build the search binary on Linux with the cuda/cuVS feature set, or unset the strict cuVS requirement for a CPU-only reference run";
const CPU_REFERENCE_REMEDIATION: &str = "set CALYX_SEARCH_ALLOW_CPU_DISKANN_REBUILD=1 only for an audited CPU reference rebuild, otherwise use cuVS CAGRA";
const DEFAULT_REBUILD_SLOT_MEMORY_BUDGET_BYTES: usize = 8 * 1024 * 1024 * 1024;
const DEFAULT_REBUILD_READER_LEASE_MS: u64 = 60 * 60 * 1000;
// Base rows repeat every persisted slot payload and can be multiple MiB each.
// Keep their transient scan window bounded independently of total corpus size.
// Slot-CF payloads use a stricter one-row point-read bound in rebuild_scan.rs.
const DEFAULT_REBUILD_SCAN_PAGE_ROWS: usize = 128;
const DEFAULT_SLOT_ROW_MEMORY_ESTIMATE_BYTES: usize = 32 * 1024;
const MIN_SLOT_MEMORY_ESTIMATE_BYTES: usize = 1024 * 1024;
const DENSE_REBUILD_MEMORY_MULTIPLIER: usize = 6;
const DENSE_ROW_OVERHEAD_BYTES: usize = 1024;
const MULTI_REBUILD_MEMORY_MULTIPLIER: usize = 2;
const MULTI_ROW_OVERHEAD_BYTES: usize = 2048;
const SPARSE_ROW_MEMORY_ESTIMATE_BYTES: usize = 4096;

#[derive(Clone, Debug)]
pub(super) struct SlotBuildPlan {
    pub(super) panel_version: u32,
    pub(super) slot: SlotId,
    pub(super) expected_ids: Vec<CxId>,
    pub(super) estimated_bytes: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct DiskAnnBuildPolicy {
    pub(super) backend: DiskAnnBuildBackend,
    pub(super) source: &'static str,
    pub(super) cuvs_compiled: bool,
}

pub(super) fn validate_parallel_rebuild_config() -> CliResult {
    configured_nonzero_usize("CALYX_SEARCH_REBUILD_MAX_PARALLEL_SLOTS")?;
    configured_nonzero_usize("RAYON_NUM_THREADS")?;
    configured_nonzero_usize("CALYX_SEARCH_REBUILD_MEMORY_BUDGET_BYTES")?;
    configured_nonzero_u64("CALYX_SEARCH_REBUILD_READER_LEASE_MS")?;
    configured_nonzero_usize("CALYX_SEARCH_REBUILD_SCAN_PAGE_ROWS")?;
    configured_diskann_build_policy()?;
    Ok(())
}

pub(super) fn configured_rebuild_reader_lease_ms() -> CliResult<u64> {
    Ok(
        configured_nonzero_u64("CALYX_SEARCH_REBUILD_READER_LEASE_MS")?
            .unwrap_or(DEFAULT_REBUILD_READER_LEASE_MS),
    )
}

pub(super) fn configured_rebuild_scan_page_rows() -> CliResult<usize> {
    Ok(
        configured_nonzero_usize("CALYX_SEARCH_REBUILD_SCAN_PAGE_ROWS")?
            .unwrap_or(DEFAULT_REBUILD_SCAN_PAGE_ROWS),
    )
}

pub(super) fn configured_diskann_build_policy() -> CliResult<DiskAnnBuildPolicy> {
    let raw_backend = match env::var(DISKANN_BUILD_BACKEND_ENV) {
        Ok(raw) => Some(raw),
        Err(env::VarError::NotPresent) => None,
        Err(env::VarError::NotUnicode(_)) => {
            return Err(CliError::usage(
                "CALYX_SEARCH_DISKANN_BUILD_BACKEND must be valid UTF-8 when set",
            ));
        }
    };
    resolve_diskann_build_policy(
        raw_backend.as_deref(),
        configured_bool(REQUIRE_CUVS_ENV)?,
        configured_bool(ALLOW_CPU_REFERENCE_ENV)?,
        calyx_sextant::CUVS_COMPILED,
    )
}

fn resolve_diskann_build_policy(
    raw_backend: Option<&str>,
    require_cuvs: bool,
    allow_cpu_reference: bool,
    cuvs_compiled: bool,
) -> CliResult<DiskAnnBuildPolicy> {
    let (backend, source) = match raw_backend {
        Some(raw) => {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                return Err(CliError::usage(
                    "CALYX_SEARCH_DISKANN_BUILD_BACKEND must not be empty",
                ));
            }
            let backend = trimmed.parse::<DiskAnnBuildBackend>().map_err(|err| {
                CliError::usage(format!(
                    "CALYX_SEARCH_DISKANN_BUILD_BACKEND must be cpu-vamana or cuvs-cagra, got {raw:?}: {err}"
                ))
            })?;
            (backend, "env")
        }
        None if cuvs_compiled => (DiskAnnBuildBackend::CuvsCagra, "compiled-cuvs-default"),
        None => (DiskAnnBuildBackend::CpuVamana, "compiled-cpu-default"),
    };
    if require_cuvs && backend != DiskAnnBuildBackend::CuvsCagra {
        return Err(calyx_error(
            CUVS_UNAVAILABLE_CODE,
            format!(
                "{REQUIRE_CUVS_ENV}=1 requires cuvs-cagra, but resolved {backend:?} from {source}"
            ),
            CUVS_REMEDIATION,
        ));
    }
    if backend == DiskAnnBuildBackend::CuvsCagra && !cuvs_compiled {
        let reason =
            calyx_sextant::cuvs_unavailable_reason("search-index rebuild cuvs-cagra backend");
        return Err(calyx_error(
            CUVS_UNAVAILABLE_CODE,
            format!(
                "{DISKANN_BUILD_BACKEND_ENV}=cuvs-cagra resolved from {source} requires cuVS CAGRA, but {reason}"
            ),
            CUVS_REMEDIATION,
        ));
    }
    if backend == DiskAnnBuildBackend::CpuVamana && cuvs_compiled && !allow_cpu_reference {
        return Err(calyx_error(
            CPU_REFERENCE_CODE,
            format!(
                "{DISKANN_BUILD_BACKEND_ENV}=cpu-vamana is a CPU reference rebuild while cuVS is compiled"
            ),
            CPU_REFERENCE_REMEDIATION,
        ));
    }
    let source = if backend == DiskAnnBuildBackend::CpuVamana && cuvs_compiled {
        "env-cpu-reference"
    } else {
        source
    };
    Ok(DiskAnnBuildPolicy {
        backend,
        source,
        cuvs_compiled,
    })
}

fn configured_bool(name: &str) -> CliResult<bool> {
    let raw = match env::var(name) {
        Ok(raw) => raw,
        Err(env::VarError::NotPresent) => return Ok(false),
        Err(env::VarError::NotUnicode(_)) => {
            return Err(CliError::usage(format!(
                "{name} must be valid UTF-8 when set"
            )));
        }
    };
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        "" => Err(CliError::usage(format!("{name} must not be empty"))),
        other => Err(CliError::usage(format!(
            "{name} must be one of 1/0/true/false/yes/no/on/off, got {other:?}"
        ))),
    }
}

fn calyx_error(
    code: &'static str,
    message: impl Into<String>,
    remediation: &'static str,
) -> CliError {
    calyx_core::CalyxError {
        code,
        message: message.into(),
        remediation,
    }
    .into()
}

pub(super) fn manifest_backend(policy: DiskAnnBuildPolicy) -> (String, String, bool) {
    (
        policy.backend.as_str().to_string(),
        policy.source.to_string(),
        policy.cuvs_compiled,
    )
}

pub(super) fn slot_build_plans(
    panel_version: u32,
    ids_by_slot: &BTreeMap<SlotId, Vec<CxId>>,
    previous_manifest: Option<&SearchIndexManifest>,
    active_slots: Option<&BTreeSet<SlotId>>,
) -> Vec<SlotBuildPlan> {
    ids_by_slot
        .iter()
        .filter(|(slot, _)| active_slots.is_none_or(|active| active.contains(slot)))
        .map(|(slot, expected_ids)| {
            let mut expected_ids = expected_ids.clone();
            expected_ids.sort();
            expected_ids.dedup();
            let estimated_bytes = estimate_slot_bytes(*slot, expected_ids.len(), previous_manifest);
            SlotBuildPlan {
                panel_version,
                slot: *slot,
                expected_ids,
                estimated_bytes,
            }
        })
        .collect()
}

pub(super) fn bounded_parallel_slot_count(plans: &[SlotBuildPlan]) -> CliResult<usize> {
    if plans.is_empty() {
        return Ok(1);
    }
    let thread_limit = configured_nonzero_usize("CALYX_SEARCH_REBUILD_MAX_PARALLEL_SLOTS")?
        .or(configured_nonzero_usize("RAYON_NUM_THREADS")?)
        .unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(|threads| threads.get())
                .unwrap_or(1)
        });
    let memory_budget = configured_nonzero_usize("CALYX_SEARCH_REBUILD_MEMORY_BUDGET_BYTES")?
        .unwrap_or(DEFAULT_REBUILD_SLOT_MEMORY_BUDGET_BYTES);
    let largest_slot = plans
        .iter()
        .map(|plan| plan.estimated_bytes)
        .max()
        .unwrap_or(MIN_SLOT_MEMORY_ESTIMATE_BYTES);
    let memory_limit = (memory_budget / largest_slot).max(1);
    Ok(thread_limit.min(memory_limit).max(1).min(plans.len()))
}

fn estimate_slot_bytes(
    slot: SlotId,
    expected_len: usize,
    previous_manifest: Option<&SearchIndexManifest>,
) -> usize {
    let Some(entry) = previous_manifest
        .and_then(|manifest| manifest.slots.iter().find(|entry| entry.slot == slot.get()))
    else {
        return expected_len
            .saturating_mul(DEFAULT_SLOT_ROW_MEMORY_ESTIMATE_BYTES)
            .max(MIN_SLOT_MEMORY_ESTIMATE_BYTES);
    };
    let estimate = match entry.kind.as_str() {
        "diskann" | "flat_dense" => entry
            .dim
            .map(|dim| {
                expected_len
                    .saturating_mul(dim as usize)
                    .saturating_mul(std::mem::size_of::<f32>())
                    .saturating_mul(DENSE_REBUILD_MEMORY_MULTIPLIER)
                    .saturating_add(expected_len.saturating_mul(DENSE_ROW_OVERHEAD_BYTES))
            })
            .unwrap_or_else(|| expected_len.saturating_mul(DEFAULT_SLOT_ROW_MEMORY_ESTIMATE_BYTES)),
        "multi_maxsim_segments" => entry
            .token_dim
            .zip(entry.token_count)
            .map(|(token_dim, token_count)| {
                token_count
                    .saturating_mul(token_dim as usize)
                    .saturating_mul(std::mem::size_of::<f32>())
                    .saturating_mul(MULTI_REBUILD_MEMORY_MULTIPLIER)
                    .saturating_add(expected_len.saturating_mul(MULTI_ROW_OVERHEAD_BYTES))
            })
            .unwrap_or_else(|| expected_len.saturating_mul(DEFAULT_SLOT_ROW_MEMORY_ESTIMATE_BYTES)),
        "sparse_inverted" => expected_len.saturating_mul(SPARSE_ROW_MEMORY_ESTIMATE_BYTES),
        _ => expected_len.saturating_mul(DEFAULT_SLOT_ROW_MEMORY_ESTIMATE_BYTES),
    };
    estimate.max(MIN_SLOT_MEMORY_ESTIMATE_BYTES)
}

fn configured_nonzero_usize(name: &str) -> CliResult<Option<usize>> {
    let raw = match env::var(name) {
        Ok(raw) => raw,
        Err(env::VarError::NotPresent) => return Ok(None),
        Err(env::VarError::NotUnicode(_)) => {
            return Err(CliError::usage(format!(
                "{name} must be valid UTF-8 when set"
            )));
        }
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(CliError::usage(format!("{name} must not be empty")));
    }
    let parsed = trimmed.parse::<usize>().map_err(|err| {
        CliError::usage(format!(
            "{name} must be a positive integer, got {raw:?}: {err}"
        ))
    })?;
    if parsed == 0 {
        return Err(CliError::usage(format!("{name} must be >= 1")));
    }
    Ok(Some(parsed))
}

fn configured_nonzero_u64(name: &str) -> CliResult<Option<u64>> {
    let raw = match env::var(name) {
        Ok(raw) => raw,
        Err(env::VarError::NotPresent) => return Ok(None),
        Err(env::VarError::NotUnicode(_)) => {
            return Err(CliError::usage(format!(
                "{name} must be valid UTF-8 when set"
            )));
        }
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(CliError::usage(format!("{name} must not be empty")));
    }
    let parsed = trimmed.parse::<u64>().map_err(|err| {
        CliError::usage(format!(
            "{name} must be a positive integer, got {raw:?}: {err}"
        ))
    })?;
    if parsed == 0 {
        return Err(CliError::usage(format!("{name} must be >= 1")));
    }
    Ok(Some(parsed))
}
