use std::collections::VecDeque;

use calyx_core::{CalyxError, LensCost, LensId, Placement, SlotResource};
use serde::{Deserialize, Serialize};

use crate::{Bgem3Engine, LensRuntime};

pub const CALYX_VRAM_BUDGET_EXCEEDED: &str = "CALYX_VRAM_BUDGET_EXCEEDED";
pub const CALYX_RAM_BUDGET_EXCEEDED: &str = "CALYX_RAM_BUDGET_EXCEEDED";
pub const CALYX_BGE_M3_CPU_GRAPH_GPU_PLACEMENT: &str = "CALYX_BGE_M3_CPU_GRAPH_GPU_PLACEMENT";
pub const LENS_VRAM_REMEDIATION: &str =
    "lower precision, evict cold GPU lenses, move to CPU, or raise the Forge VRAM budget";
pub const LENS_RAM_REMEDIATION: &str =
    "raise the CPU lens pool cap or evict cold resident CPU lenses";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlacementBudget {
    pub vram_soft_cap_bytes: u64,
    pub tei_reserved_bytes: u64,
    pub vram_allocated_bytes: u64,
    pub ram_soft_cap_bytes: u64,
    pub ram_used_bytes: u64,
    pub cpu_resident_limit: usize,
    pub cpu_resident_count: usize,
}

impl PlacementBudget {
    pub fn available_vram_bytes(&self) -> u64 {
        self.vram_soft_cap_bytes
            .saturating_sub(self.tei_reserved_bytes)
            .saturating_sub(self.vram_allocated_bytes)
    }

    pub fn available_ram_bytes(&self) -> u64 {
        self.ram_soft_cap_bytes.saturating_sub(self.ram_used_bytes)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PlacementPlan {
    pub resource: SlotResource,
    pub reason: String,
    pub available_vram_bytes: u64,
    pub available_ram_bytes: u64,
}

pub fn choose_placement(
    runtime: &LensRuntime,
    cost: LensCost,
    budget: PlacementBudget,
) -> Result<PlacementPlan, CalyxError> {
    if matches!(
        runtime,
        LensRuntime::FastembedBgem3 {
            engine: Bgem3Engine::FastembedCpu,
            ..
        }
    ) {
        if cost.vram_bytes != 0 {
            return Err(CalyxError {
                code: CALYX_BGE_M3_CPU_GRAPH_GPU_PLACEMENT,
                message: format!(
                    "CPU-only FastEmbed BGE-M3 graph declares {} VRAM bytes and would be falsely admitted as GPU-resident",
                    cost.vram_bytes
                ),
                remediation: "commission the pinned onnx-bgem3-* joint CUDA artifact; do not relabel the CPU INT8 graph or fall back to CPU",
            });
        }
        ensure_cpu_budget(cost, budget)?;
        return Ok(plan(
            cost,
            Placement::Cpu,
            budget,
            "explicit legacy CPU-only BGE-M3 runtime",
        ));
    }

    if algorithmic_cuda_capable(runtime) && budget.vram_soft_cap_bytes > 0 {
        return Ok(plan(
            cost,
            Placement::Gpu,
            budget,
            "bulk algorithmic runtime uses dynamic CUDA/CPU crossover dispatch",
        ));
    }

    if cost.is_zero_cost() && matches!(runtime, LensRuntime::Algorithmic { .. }) {
        return Ok(plan(cost, Placement::Cpu, budget, "zero-cost lens admits"));
    }

    if runtime_prefers_cpu(runtime) {
        ensure_cpu_budget(cost, budget)?;
        return Ok(plan(cost, Placement::Cpu, budget, "CPU-native runtime"));
    }

    if cost.vram_bytes <= budget.available_vram_bytes() {
        return Ok(plan(
            cost,
            Placement::Gpu,
            budget,
            "fits GPU budget after TEI reservation",
        ));
    }

    Err(vram_budget_error(format!(
        "lens requires {} VRAM bytes, available {} after TEI reservation {} and allocated {}",
        cost.vram_bytes,
        budget.available_vram_bytes(),
        budget.tei_reserved_bytes,
        budget.vram_allocated_bytes
    )))
}

fn plan(
    cost: LensCost,
    placement: Placement,
    budget: PlacementBudget,
    reason: &str,
) -> PlacementPlan {
    PlacementPlan {
        resource: SlotResource { cost, placement },
        reason: reason.to_string(),
        available_vram_bytes: budget.available_vram_bytes(),
        available_ram_bytes: budget.available_ram_bytes(),
    }
}

fn runtime_prefers_cpu(runtime: &LensRuntime) -> bool {
    match runtime {
        LensRuntime::Algorithmic { .. } => !algorithmic_cuda_capable(runtime),
        LensRuntime::MultimodalAdapter { .. }
        | LensRuntime::StaticLookup { .. }
        | LensRuntime::ExternalCmd { .. } => true,
        _ => false,
    }
}

fn algorithmic_cuda_capable(runtime: &LensRuntime) -> bool {
    let LensRuntime::Algorithmic { kind } = runtime else {
        return false;
    };
    matches!(
        kind.as_str(),
        "byte"
            | "byte-features"
            | "byte_features"
            | "sparse"
            | "sparse-keywords"
            | "sparse_keywords"
            | "sparse-keywords-tf"
            | "sparse_keywords_tf"
            | "token-hash"
            | "token_hash"
            | "multi-hash"
            | "multi_hash"
    ) || kind.starts_with("sparse-keywords:")
        || kind.starts_with("sparse_keywords:")
        || kind.starts_with("sparse-keywords-tf:")
        || kind.starts_with("sparse_keywords_tf:")
        || kind.starts_with("token-hash:")
        || kind.starts_with("token_hash:")
}

fn ensure_cpu_budget(cost: LensCost, budget: PlacementBudget) -> Result<(), CalyxError> {
    if cost.is_zero_cost() {
        return Ok(());
    }
    if budget.cpu_resident_count >= budget.cpu_resident_limit {
        return Err(ram_budget_error(format!(
            "CPU lens pool full: resident_count={} limit={}",
            budget.cpu_resident_count, budget.cpu_resident_limit
        )));
    }
    if cost.ram_bytes > budget.available_ram_bytes() {
        return Err(ram_budget_error(format!(
            "lens requires {} RAM bytes, available {}",
            cost.ram_bytes,
            budget.available_ram_bytes()
        )));
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CpuPoolAdmission {
    pub evicted_lenses: Vec<LensId>,
    pub resident_lenses: usize,
    pub resident_ram_bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CpuResidentLens {
    lens_id: LensId,
    ram_bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CpuLensPool {
    resident_limit: usize,
    ram_soft_cap_bytes: u64,
    resident: VecDeque<CpuResidentLens>,
}

impl CpuLensPool {
    pub fn new(resident_limit: usize, ram_soft_cap_bytes: u64) -> Self {
        Self {
            resident_limit,
            ram_soft_cap_bytes,
            resident: VecDeque::new(),
        }
    }

    pub fn admit(
        &mut self,
        lens_id: LensId,
        cost: LensCost,
    ) -> Result<CpuPoolAdmission, CalyxError> {
        if cost.is_zero_cost() {
            return Ok(self.admission(Vec::new()));
        }
        if self.resident_limit == 0 || cost.ram_bytes > self.ram_soft_cap_bytes {
            return Err(ram_budget_error(format!(
                "lens {lens_id} requires {} RAM bytes with pool_limit={} ram_cap={}",
                cost.ram_bytes, self.resident_limit, self.ram_soft_cap_bytes
            )));
        }

        self.remove(lens_id);
        let mut evicted = Vec::new();
        while self.resident.len() >= self.resident_limit
            || self.resident_ram_bytes().saturating_add(cost.ram_bytes) > self.ram_soft_cap_bytes
        {
            let Some(old) = self.resident.pop_front() else {
                break;
            };
            evicted.push(old.lens_id);
        }

        self.resident.push_back(CpuResidentLens {
            lens_id,
            ram_bytes: cost.ram_bytes,
        });
        Ok(self.admission(evicted))
    }

    pub fn resident_ram_bytes(&self) -> u64 {
        self.resident
            .iter()
            .map(|entry| entry.ram_bytes)
            .fold(0_u64, u64::saturating_add)
    }

    pub fn resident_lenses(&self) -> usize {
        self.resident.len()
    }

    fn remove(&mut self, lens_id: LensId) {
        if let Some(index) = self
            .resident
            .iter()
            .position(|entry| entry.lens_id == lens_id)
        {
            self.resident.remove(index);
        }
    }

    fn admission(&self, evicted_lenses: Vec<LensId>) -> CpuPoolAdmission {
        CpuPoolAdmission {
            evicted_lenses,
            resident_lenses: self.resident_lenses(),
            resident_ram_bytes: self.resident_ram_bytes(),
        }
    }
}

fn vram_budget_error(message: String) -> CalyxError {
    CalyxError {
        code: CALYX_VRAM_BUDGET_EXCEEDED,
        message,
        remediation: LENS_VRAM_REMEDIATION,
    }
}

fn ram_budget_error(message: String) -> CalyxError {
    CalyxError {
        code: CALYX_RAM_BUDGET_EXCEEDED,
        message,
        remediation: LENS_RAM_REMEDIATION,
    }
}
