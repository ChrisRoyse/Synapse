use crate::Registry;
use crate::panels::{PanelLensRuntime, PanelTemplate, instantiate_panel};
use crate::profile::{CapabilityGateDecision, CapabilityGateEvaluation};
use crate::spec::LensHealth;
use crate::swap::{LifecycleOutcome, SwapController};
use calyx_assay::store::{AssayCacheKey, AssayStore, AssaySubject};
use calyx_aster::MAX_DURABLE_SLOT_ID;
use calyx_core::{
    CalyxError, LensId, Modality, Panel, QuantPolicy, Slot, SlotId, SlotKey, SlotResource,
    SlotState, Ts,
};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PanelDiff {
    pub added: Vec<SlotId>,
    pub retired: Vec<SlotId>,
    pub unchanged: Vec<SlotId>,
    pub panel_version: u32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PanelSlotListing {
    pub slot_id: SlotId,
    pub key: String,
    pub lens_id: LensId,
    pub state: SlotState,
    pub quant: QuantPolicy,
    pub resource: SlotResource,
    pub bits_about: Option<f32>,
    pub health: LensHealth,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PanelCapabilityGateOutcome {
    pub slot_id: SlotId,
    pub lens_id: LensId,
    pub decision: CapabilityGateDecision,
    pub state: SlotState,
    pub panel_version: u32,
    pub reason: String,
}

pub const CALYX_PANEL_LENS_MISSING: &str = "CALYX_PANEL_LENS_MISSING";
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AppliedPanelTemplate {
    pub template_name: String,
    pub diff: PanelDiff,
    pub resolved_lenses: Vec<ResolvedPanelLens>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedPanelLens {
    pub slot_key: String,
    pub lens_name: String,
    pub lens_id: LensId,
}

pub fn list_panel(panel: &Panel, registry: &Registry) -> Vec<PanelSlotListing> {
    panel
        .slots
        .iter()
        .map(|slot| listing_for_slot(slot, registry))
        .collect()
}

pub fn apply_panel_template(
    panel: &mut Panel,
    registry: &Registry,
    template: &PanelTemplate,
    now: Ts,
    next_panel_generation: u32,
) -> calyx_core::Result<AppliedPanelTemplate> {
    let mut target = instantiate_panel(template, now);
    let resolved_lenses = resolve_registry_slots(&mut target.panel, registry, template)?;
    let diff = swap_panel_to_target(panel, &target.panel, now, next_panel_generation)?;
    Ok(AppliedPanelTemplate {
        template_name: template.name.clone(),
        diff,
        resolved_lenses,
    })
}

pub fn apply_capability_gate(
    controller: &mut SwapController,
    slot_id: SlotId,
    evaluation: &CapabilityGateEvaluation,
    now: Ts,
    next_panel_generation: u32,
) -> calyx_core::Result<PanelCapabilityGateOutcome> {
    let slot = controller
        .panel()
        .slots
        .iter()
        .find(|slot| slot.slot_id == slot_id)
        .ok_or_else(|| CalyxError::registry_unavailable(format!("slot {slot_id} not in panel")))?;
    if slot.lens_id != evaluation.lens_id {
        return Err(CalyxError::registry_unavailable(format!(
            "capability gate lens {} does not match slot {} lens {}",
            evaluation.lens_id, slot_id, slot.lens_id
        )));
    }

    let lifecycle = match evaluation.decision {
        CapabilityGateDecision::Admit if slot.state == SlotState::Active => LifecycleOutcome {
            slot_id,
            lens_id: slot.lens_id,
            state: slot.state,
            panel_version: controller.panel().version,
        },
        CapabilityGateDecision::Admit => {
            controller.unpark_lens(slot_id, now, next_panel_generation)?
        }
        CapabilityGateDecision::Park => {
            controller.park_lens(slot_id, now, next_panel_generation)?
        }
        CapabilityGateDecision::Retire => {
            controller.retire_lens(slot_id, now, next_panel_generation)?
        }
    };

    Ok(PanelCapabilityGateOutcome {
        slot_id,
        lens_id: lifecycle.lens_id,
        decision: evaluation.decision,
        state: lifecycle.state,
        panel_version: lifecycle.panel_version,
        reason: evaluation.reason.clone(),
    })
}

pub fn list_panel_with_assay(
    panel: &Panel,
    registry: &Registry,
    assay_store: &AssayStore,
    cache_key: &AssayCacheKey,
) -> Vec<PanelSlotListing> {
    panel
        .slots
        .iter()
        .map(|slot| {
            let mut listing = listing_for_slot(slot, registry);
            if let Some(row) =
                assay_store.get(cache_key, &AssaySubject::Lens { slot: slot.slot_id })
            {
                listing.bits_about = Some(row.estimate.bits);
            }
            listing
        })
        .collect()
}

pub fn swap_panel(
    panel: &mut Panel,
    template: &PanelTemplate,
    now: Ts,
    next_panel_generation: u32,
) -> calyx_core::Result<PanelDiff> {
    let target = instantiate_panel(template, now);
    swap_panel_to_target(panel, &target.panel, now, next_panel_generation)
}

pub fn swap_panel_to_target(
    panel: &mut Panel,
    target: &Panel,
    now: Ts,
    next_panel_generation: u32,
) -> calyx_core::Result<PanelDiff> {
    let mut proposed = panel.clone();
    let diff = swap_panel_to_target_inner(&mut proposed, target, now, next_panel_generation)?;
    *panel = proposed;
    Ok(diff)
}

fn swap_panel_to_target_inner(
    panel: &mut Panel,
    target: &Panel,
    now: Ts,
    next_panel_generation: u32,
) -> calyx_core::Result<PanelDiff> {
    let target_ids = target
        .slots
        .iter()
        .map(|slot| slot.lens_id)
        .collect::<Vec<_>>();
    let mut added = Vec::new();
    let mut retired = Vec::new();
    let mut unchanged = Vec::new();

    for slot in &mut panel.slots {
        if target_ids.contains(&slot.lens_id) && slot.state != SlotState::Retired {
            unchanged.push(slot.slot_id);
        } else if slot.state != SlotState::Retired {
            slot.state = SlotState::Retired;
            retired.push(slot.slot_id);
        }
    }

    let mut occupied = panel
        .slots
        .iter()
        .map(|slot| slot.slot_id.get())
        .collect::<std::collections::BTreeSet<_>>();
    for target_slot in &target.slots {
        let exists = panel
            .slots
            .iter()
            .any(|slot| slot.lens_id == target_slot.lens_id && slot.state != SlotState::Retired);
        if exists {
            continue;
        }
        let slot_id = (0..=MAX_DURABLE_SLOT_ID)
            .find(|candidate| !occupied.contains(candidate))
            .map(SlotId::new)
            .ok_or_else(|| {
                CalyxError::lens_frozen_violation(format!(
                    "panel generation {} has exhausted all {} durable slot identifiers (0..={MAX_DURABLE_SLOT_ID}); retired identifiers remain reserved for historical decoding",
                    panel.version,
                    u32::from(MAX_DURABLE_SLOT_ID) + 1
                ))
            })?;
        occupied.insert(slot_id.get());
        panel.slots.push(cloned_target_slot(target_slot, slot_id));
        added.push(slot_id);
    }

    if !added.is_empty() || !retired.is_empty() {
        if next_panel_generation <= panel.version {
            return Err(CalyxError::lens_frozen_violation(format!(
                "allocator-issued panel generation {next_panel_generation} must be greater than current generation {}; allocate a vault-global generation before applying a panel template",
                panel.version
            )));
        }
        panel.version = next_panel_generation;
        panel.created_at = now;
        for slot in &mut panel.slots {
            if added.contains(&slot.slot_id) {
                slot.added_at_panel_version = panel.version;
            }
        }
    }

    Ok(PanelDiff {
        added,
        retired,
        unchanged,
        panel_version: panel.version,
    })
}

fn resolve_registry_slots(
    panel: &mut Panel,
    registry: &Registry,
    template: &PanelTemplate,
) -> calyx_core::Result<Vec<ResolvedPanelLens>> {
    let mut resolved = Vec::new();
    for (slot, spec) in panel.slots.iter_mut().zip(&template.slots) {
        let PanelLensRuntime::Registry { name } = &spec.runtime else {
            continue;
        };
        let lens_id = registry
            .find_lens_by_name(name)
            .ok_or_else(|| panel_lens_missing(&template.name, &spec.name, name))?;
        let contract = registry
            .frozen_contract(lens_id)
            .ok_or_else(|| panel_lens_missing(&template.name, &spec.name, name))?;
        if contract.shape() != spec.output {
            return Err(CalyxError::lens_dim_mismatch(format!(
                "panel {} slot {} shape {:?} != lens {} frozen {:?}",
                template.name,
                spec.name,
                spec.output,
                name,
                contract.shape()
            )));
        }
        if contract.modality() != spec.modality {
            return Err(CalyxError::lens_dim_mismatch(format!(
                "panel {} slot {} modality {:?} != lens {} frozen {:?}",
                template.name,
                spec.name,
                spec.modality,
                name,
                contract.modality()
            )));
        }
        slot.lens_id = lens_id;
        slot.quant = registry
            .lens_spec(lens_id)
            .map_or(QuantPolicy::None, |spec| spec.quant_default);
        resolved.push(ResolvedPanelLens {
            slot_key: spec.name.clone(),
            lens_name: name.clone(),
            lens_id,
        });
    }
    Ok(resolved)
}

fn panel_lens_missing(template: &str, slot: &str, lens_name: &str) -> CalyxError {
    CalyxError {
        code: CALYX_PANEL_LENS_MISSING,
        message: format!(
            "panel {template} slot {slot} references missing or unconverted lens {lens_name}"
        ),
        remediation: "convert and register the missing lens before applying the panel",
    }
}

fn cloned_target_slot(target: &Slot, slot_id: SlotId) -> Slot {
    let mut slot = target.clone();
    slot.slot_id = slot_id;
    slot.slot_key = SlotKey::new(slot_id, target.slot_key.key().to_string());
    slot
}

fn listing_for_slot(slot: &Slot, registry: &Registry) -> PanelSlotListing {
    PanelSlotListing {
        slot_id: slot.slot_id,
        key: slot.slot_key.key().to_string(),
        lens_id: slot.lens_id,
        state: slot.state,
        quant: slot.quant,
        resource: slot.resource.clone(),
        bits_about: slot_bits(slot),
        health: registry
            .health(slot.lens_id)
            .unwrap_or_else(|err| missing_slot_health(slot, err)),
    }
}

fn missing_slot_health(slot: &Slot, err: CalyxError) -> LensHealth {
    if is_builtin_temporal_slot(slot) {
        return LensHealth::Loaded;
    }
    LensHealth::Failing {
        code: "CALYX_LENS_UNREACHABLE".to_string(),
        reason: err.message,
    }
}

fn is_builtin_temporal_slot(slot: &Slot) -> bool {
    slot.modality == Modality::Structured
        && slot.retrieval_only
        && slot.excluded_from_dedup
        && matches!(
            slot.slot_key.key(),
            "E2_recency" | "E3_periodic" | "E4_positional"
        )
}

fn slot_bits(slot: &Slot) -> Option<f32> {
    slot.bits_about
        .values()
        .map(|signal| signal.bits)
        .max_by(|left, right| left.total_cmp(right))
}
