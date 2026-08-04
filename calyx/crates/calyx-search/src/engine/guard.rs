use std::collections::BTreeMap;

use calyx_aster::cf::ColumnFamily;
use calyx_aster::vault::AsterVault;
use calyx_core::CalyxError;
use calyx_core::{Clock, Constellation, CxId, SlotId, SlotVector, VaultStore};
use calyx_sextant::Hit;
use calyx_ward::GuardProfile;

use crate::engine_trace::SearchTracer;
use crate::error::CliResult;

use super::GuardChoice;
use super::support::guard_cosine;

/// Guard CF key of the default calibrated profile (MCP parity).
pub const DEFAULT_GUARD_PROFILE_KEY: &[u8] = b"profile\0default";

/// How the in-region guard will actually be enforced for this search (#1094).
///
/// `--guard in-region` WITHOUT an operator tau is profile-backed: the
/// calibrated Ward [`GuardProfile`] is loaded from the Guard CF and applied
/// with per-slot conformal taus (MCP parity). An explicit operator tau keeps
/// the flat single-tau cosine gate with its provenance recorded in the
/// outcome (#1088 probe-matrix calibration path). There is no silent default
/// tau anywhere: a missing profile fails closed with
/// `CALYX_GUARD_PROVISIONAL`.
pub(super) enum ResolvedGuard {
    Off,
    OperatorTau(f32),
    Profile(Box<GuardProfile>),
}

/// Resolve the guard mode. `guard = Off` rejects a supplied tau (usage
/// error). For `in-region`, an operator-supplied tau must be finite and in
/// `(0.0, 1.0]` - cosine similarity is bounded by 1.0 and a non-positive
/// threshold accepts everything, defeating the guard. Without a tau the
/// calibrated Ward profile is loaded from the Guard CF and fails closed when
/// absent/uncalibrated/panel-mismatched. Never silently clamp or default
/// (issues #1088, #1094).
pub(super) fn resolve_guard<C: Clock>(
    vault: &AsterVault<C>,
    guard: GuardChoice,
    guard_tau: Option<f32>,
    guard_panel_version: Option<u32>,
) -> CliResult<ResolvedGuard> {
    match (guard, guard_tau) {
        (GuardChoice::Off, None) => Ok(ResolvedGuard::Off),
        (GuardChoice::Off, Some(_)) => Err(crate::error::SearchError::usage(
            "guard tau was supplied but guard mode is not in-region; pass --guard in-region or omit the tau",
        )),
        (GuardChoice::InRegion, Some(tau)) if !tau.is_finite() || tau <= 0.0 || tau > 1.0 => {
            Err(crate::error::SearchError::usage(format!(
                "in-region guard tau {tau} is out of range; supply a finite cosine threshold in (0.0, 1.0]"
            )))
        }
        (GuardChoice::InRegion, Some(tau)) => Ok(ResolvedGuard::OperatorTau(tau)),
        (GuardChoice::InRegion, None) => Ok(ResolvedGuard::Profile(Box::new(
            load_default_guard_profile(vault, guard_panel_version)?,
        ))),
    }
}

/// Load the calibrated Ward profile for the requested panel from the Guard CF.
/// The default active-panel key is only a compatibility mirror. Missing,
/// undecodable, uncalibrated, or panel-mismatched state fails closed with
/// `CALYX_GUARD_PROVISIONAL`.
///
/// The caller must have opened the vault with [`ColumnFamily::Guard`]
/// selected: reading an unselected CF silently returns `None`, which would
/// masquerade as a missing profile (#1094).
fn load_default_guard_profile<C: Clock>(
    vault: &AsterVault<C>,
    guard_panel_version: Option<u32>,
) -> CliResult<GuardProfile> {
    let panel_key = guard_panel_version.map(|panel_version| {
        let mut key = Vec::with_capacity(19);
        key.extend_from_slice(b"profile\0panel\0");
        key.extend_from_slice(&panel_version.to_be_bytes());
        key
    });
    let snapshot = vault.snapshot();
    let panel_profile = match panel_key.as_deref() {
        Some(key) => vault.read_cf_at(snapshot, ColumnFamily::Guard, key)?,
        None => None,
    };
    let default_profile = match panel_profile {
        Some(bytes) => Some(bytes),
        None => vault.read_cf_at(snapshot, ColumnFamily::Guard, DEFAULT_GUARD_PROFILE_KEY)?,
    };
    let Some(bytes) = default_profile else {
        return Err(CalyxError::guard_provisional(
            "in-region search requires a calibrated guard profile for the requested panel (Guard CF key `profile\\0panel\\0<version>`, with `profile\\0default` only as the active-panel compatibility mirror); calibrate that panel or supply an explicit operator tau",
        )
        .into());
    };
    let profile: GuardProfile = serde_json::from_slice(&bytes)
        .map_err(|error| CalyxError::guard_provisional(format!("decode guard profile: {error}")))?;
    if let Some(panel_version) = guard_panel_version
        && profile.panel_version != panel_version
    {
        return Err(CalyxError::guard_provisional(format!(
            "guard profile panel_version {} does not match requested panel {panel_version}; recalibrate the guard for that panel",
            profile.panel_version
        ))
        .into());
    }
    if !profile.is_calibrated() {
        return Err(CalyxError::guard_provisional(
            "guard profile is not calibrated; run `calyx guard <vault> calibrate` for the requested panel",
        )
        .into());
    }
    Ok(profile)
}

pub(super) fn prefilter_in_region_candidates_traced(
    hits: Vec<Hit>,
    query_vectors: &[(SlotId, SlotVector)],
    tau: f32,
    trace: &mut SearchTracer<'_>,
) -> Vec<Hit> {
    let mut kept = Vec::new();
    for hit in hits {
        let best = prefilter_best_score(&hit, query_vectors);
        let accepted = best.is_some_and(|value| value >= tau);
        trace.emit_detail(
            "guard.prefilter.candidate",
            None,
            Some(hit.rank),
            Some(format!(
                "cx_id={} tau={tau:.6} best_index_score={} kept={accepted}",
                hit.cx_id,
                best.map(|value| format!("{value:.6}"))
                    .unwrap_or_else(|| "missing".to_string())
            )),
        );
        if accepted {
            kept.push(hit);
        }
    }
    kept
}

pub(super) fn apply_in_region_guard_traced(
    hits: Vec<Hit>,
    docs: &BTreeMap<CxId, Constellation>,
    query_vectors: &[(SlotId, SlotVector)],
    tau: f32,
    trace: &mut SearchTracer<'_>,
) -> Vec<Hit> {
    let mut kept = Vec::new();
    for hit in hits {
        let best = guard_cosine(&hit, docs, query_vectors);
        let accepted = best.is_some_and(|value| value >= tau);
        trace.emit_detail(
            "guard.in_region.candidate",
            None,
            Some(hit.rank),
            Some(format!(
                "cx_id={} tau={tau:.6} best_cosine={} kept={accepted}",
                hit.cx_id,
                best.map(|value| format!("{value:.6}"))
                    .unwrap_or_else(|| "missing".to_string())
            )),
        );
        if accepted {
            kept.push(hit);
        }
    }
    kept
}

fn prefilter_best_score(hit: &Hit, query_vectors: &[(SlotId, SlotVector)]) -> Option<f32> {
    hit.per_lens
        .iter()
        .filter_map(|item| {
            let has_dense_query = query_vectors
                .iter()
                .any(|(slot, vector)| *slot == item.slot.slot_id() && vector.as_dense().is_some());
            (has_dense_query && item.raw_score.is_finite()).then_some(item.raw_score)
        })
        .max_by(f32::total_cmp)
}
