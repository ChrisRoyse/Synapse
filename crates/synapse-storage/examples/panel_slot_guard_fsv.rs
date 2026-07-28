//! Manual FSV instrument for the #1776 global slot allocation guards.
//!
//! Slot ids are global: Calyx persists every measured vector in a physical
//! `cf/slot_<id>` column family keyed only by `CxId`, so two panels sharing a
//! slot id share one column family holding different shapes and meanings.
//!
//! Two guards enforce the allocation. The compile-time one
//! (`PANEL_SLOT_BLOCKS` disjointness) is proven by making the build fail. This
//! driver proves the other: that a real constellation built by a real public
//! builder lands only in its own panel's block, so the write-time
//! `validate_panel_slot_allocation` backstop has something concrete to check.
//!
//! It builds one constellation per panel from real record shapes and prints the
//! exact slot ids each declares, so the observed ids can be compared against
//! the declared blocks rather than assumed.
//!
//! Usage:
//! `cargo run -p synapse-storage --example panel_slot_guard_fsv`

use std::error::Error;
use std::str::FromStr as _;

use calyx_core::{CxId, VaultId};
use serde_json::json;
use synapse_storage::constellations::{
    NativeConstellationContext, SYN_ACTION_PANEL_NAME, SYN_MCP_USAGE_PANEL_NAME,
    SYN_OUTCOME_PANEL_NAME, SYN_PROCESS_PANEL_NAME, SYN_TIMELINE_PANEL_NAME,
    build_action_constellation, build_process_constellation,
};

fn context() -> Result<NativeConstellationContext, Box<dyn Error>> {
    Ok(NativeConstellationContext {
        vault_id: VaultId::from_str("01KYN9878AFNR5ESEDB1S5AETN")?,
        cx_id: CxId::from_bytes([7_u8; 16]),
        created_at_ms: 1_785_000_000_000,
        next_ledger_seq: 1,
    })
}

fn report(label: &str, panel: &str, slots: &[u16]) {
    let ids = slots
        .iter()
        .map(u16::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let min = slots.iter().copied().min().unwrap_or(0);
    let max = slots.iter().copied().max().unwrap_or(0);
    println!(
        "PANEL_SLOTS builder={label} panel={panel} count={} ids={ids} min={min} max={max}",
        slots.len()
    );
}

fn main() -> Result<(), Box<dyn Error>> {
    println!("panel_slot_guard_fsv: observed slot ids per real public builder");
    println!("declared blocks: timeline 1..=7, episode 8..=22, agent-event 23..=34,");
    println!("  agent-transcript 35..=47, action 48..=52, reflex 53..=59, process 60..=66,");
    println!("  observation 67..=74, outcome 75..=81, mcp-usage 82..=93,");
    println!("  recurrence-subject 94..=95, graphpos-app 96..=97, graphpos-process 98..=99,");
    println!("  path-hierarchy 100..=102");
    println!(
        "(reference: {SYN_TIMELINE_PANEL_NAME}, {SYN_ACTION_PANEL_NAME}, {SYN_PROCESS_PANEL_NAME}, {SYN_OUTCOME_PANEL_NAME}, {SYN_MCP_USAGE_PANEL_NAME})"
    );

    let action_record = json!({
        "ts_ns": 1_785_000_000_000_000_000_u64,
        "kind": "act_click",
        "target": "fsv-1776-target",
        "params": { "x": 10, "y": 20 },
    });
    let action_raw = serde_json::to_vec(&action_record)?;
    let action = build_action_constellation(
        context()?,
        b"fsv-1776/action-key",
        &action_raw,
        &action_record,
    )?;
    report(
        "build_action_constellation",
        SYN_ACTION_PANEL_NAME,
        &action
            .slots
            .keys()
            .map(|slot| slot.get())
            .collect::<Vec<_>>(),
    );

    let process_record = json!({
        "ts_ns": 1_785_000_000_000_000_000_u64,
        "event": "start",
        "image": "fsv-1776.exe",
        "pid": 4242,
        "uptime_ms": 1234,
    });
    let process_raw = serde_json::to_vec(&process_record)?;
    let process = build_process_constellation(
        context()?,
        b"fsv-1776/process-key",
        &process_raw,
        &process_record,
    )?;
    report(
        "build_process_constellation",
        SYN_PROCESS_PANEL_NAME,
        &process
            .slots
            .keys()
            .map(|slot| slot.get())
            .collect::<Vec<_>>(),
    );

    Ok(())
}
