use calyx_core::{CalyxError, LensId, PanelSlotId};
use calyx_ledger::LedgerCfStore;
use sha2::{Digest, Sha256};

use crate::{
    AnnealFaultLedgerDetails, AnnealLedger, AnnealLedgerAction, AnnealLedgerEntry,
    CALYX_ANNEAL_BUDGET_EXHAUSTED, ComponentKind, MetricSnapshot, ScopeId,
};

use super::{CALYX_ANNEAL_FAULT_INVALID_EVENT, FaultEvent};

const MAX_BACKOFF_TICKS: u32 = 256;

pub(super) fn component_details(component: &ComponentKind) -> AnnealFaultLedgerDetails {
    let component_hash = hex_bytes(blake3::hash(&component.storage_key()).as_bytes());
    match component {
        ComponentKind::AnnIndex { panel_slot } => {
            details("ann_index", component_hash).with_slot(*panel_slot)
        }
        ComponentKind::GuardProfile { panel_slot } => {
            details("guard_profile", component_hash).with_slot(*panel_slot)
        }
        ComponentKind::LensEndpoint { lens_id } => {
            details("lens_endpoint", component_hash).with_lens(*lens_id)
        }
        ComponentKind::KernelIndex { scope } => {
            details("kernel_index", component_hash).with_scope_hash(scope)
        }
        ComponentKind::BaseShard { shard_id } => {
            details("base_shard", component_hash).with_shard_id(shard_id)
        }
    }
}

pub(super) fn sha256(bytes: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher.finalize().into()
}

pub(super) fn backoff_ticks(failures: u32) -> u32 {
    let shift = failures.saturating_sub(1).min(8);
    1_u32.checked_shl(shift).unwrap_or(MAX_BACKOFF_TICKS)
}

pub(super) fn budget_exhausted() -> CalyxError {
    CalyxError {
        code: CALYX_ANNEAL_BUDGET_EXHAUSTED,
        message: "fault monitor background budget exhausted".to_string(),
        remediation: "increase PH43 anneal background budget or reduce detector cadence",
    }
}

pub(super) fn write_fault_event<L, C>(
    ledger: &mut AnnealLedger<L, C>,
    event: &FaultEvent,
) -> calyx_core::Result<()>
where
    L: LedgerCfStore,
    C: calyx_core::Clock,
{
    let details = event.ledger_details();
    let details_bytes = serde_json::to_vec(&details).map_err(invalid_event)?;
    ledger
        .write(AnnealLedgerEntry {
            action: AnnealLedgerAction::FaultEvent,
            change_id: event.change_id(),
            artifact_id: details.component_hash.clone(),
            prior_ptr_hash: *blake3::hash(event.component.storage_key().as_slice()).as_bytes(),
            candidate_ptr_hash: *blake3::hash(&details_bytes).as_bytes(),
            metrics: MetricSnapshot::empty(event.observed_at),
            ts: event.observed_at,
            description: format!("fault event {}", event.fault_kind.as_str()),
            fault: Some(details),
            proposal: None,
            details: None,
            prev_hash: None,
        })
        .map(|_| ())
}

pub(super) fn invalid_event(error: serde_json::Error) -> CalyxError {
    CalyxError {
        code: CALYX_ANNEAL_FAULT_INVALID_EVENT,
        message: format!("encode fault event ledger details: {error}"),
        remediation: "repair fault-event serialization before writing ledger",
    }
}

fn details(kind: &str, component_hash: String) -> AnnealFaultLedgerDetails {
    AnnealFaultLedgerDetails {
        fault_kind: String::new(),
        recommendation: String::new(),
        component_kind: kind.to_string(),
        component_hash,
        panel_version: None,
        slot_id: None,
        lens_id: None,
        scope_hash: None,
        shard_id: None,
    }
}

trait DetailExt {
    fn with_slot(self, panel_slot: PanelSlotId) -> Self;
    fn with_lens(self, lens_id: LensId) -> Self;
    fn with_shard_id(self, value: &str) -> Self;
    fn with_scope_hash(self, scope: &ScopeId) -> Self;
}

impl DetailExt for AnnealFaultLedgerDetails {
    fn with_slot(mut self, panel_slot: PanelSlotId) -> Self {
        self.panel_version = Some(panel_slot.panel_version());
        self.slot_id = Some(panel_slot.slot_id().get());
        self
    }

    fn with_lens(mut self, lens_id: LensId) -> Self {
        self.lens_id = Some(lens_id.to_string());
        self
    }

    fn with_shard_id(mut self, value: &str) -> Self {
        self.shard_id = Some(value.to_string());
        self
    }

    fn with_scope_hash(mut self, scope: &ScopeId) -> Self {
        self.scope_hash = Some(hex_bytes(
            blake3::hash(scope.to_string().as_bytes()).as_bytes(),
        ));
        self
    }
}

fn hex_bytes(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(hex_digit(byte >> 4));
        out.push(hex_digit(byte & 0x0f));
    }
    out
}

fn hex_digit(value: u8) -> char {
    match value {
        0..=9 => char::from(b'0' + value),
        10..=15 => char::from(b'a' + value - 10),
        _ => unreachable!("nibble out of range"),
    }
}
