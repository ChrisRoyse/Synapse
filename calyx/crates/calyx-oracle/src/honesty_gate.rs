//! Oracle honesty gate backed by Assay sufficiency rows.

use std::collections::BTreeMap;

use calyx_assay::{
    AssayCacheKey, AssayRow, AssayStore, AssaySubject, DeficitRoutingContext, EstimatorKind,
    PanelSufficiency, TrustTag, panel_sufficiency_from_estimate_with_context,
    per_sensor_attribution,
};
use calyx_aster::vault::AsterVault;
use calyx_core::{AnchorKind, CalyxError, Clock, LensId, Panel, SlotId, VaultId};

use crate::{Bits, DomainId, OracleError, SufficiencyBound, UnitInterval};

const SOLE_CARRIER_BITS: f32 = 0.10;

pub trait SufficiencyAssay {
    fn panel_sufficiency(
        &self,
        panel: &Panel,
        domain: &DomainId,
        clock: &dyn Clock,
    ) -> Result<PanelSufficiency, OracleError>;
}

pub fn check_sufficiency<C>(
    vault: &AsterVault<C>,
    panel: &Panel,
    domain: DomainId,
    clock: &dyn Clock,
) -> Result<SufficiencyBound, OracleError>
where
    C: Clock,
{
    let assay = VaultSufficiencyAssay::new(vault);
    check_sufficiency_with_assay(&assay, panel, domain, clock)
}

pub fn check_sufficiency_with_assay<A>(
    assay: &A,
    panel: &Panel,
    domain: DomainId,
    clock: &dyn Clock,
) -> Result<SufficiencyBound, OracleError>
where
    A: SufficiencyAssay,
{
    let report = assay.panel_sufficiency(panel, &domain, clock)?;
    let report_bits = validate_report(&report)?;
    let sufficient = report_bits.sufficiency_basis >= report_bits.anchor_entropy;
    let per_sensor_deficit = if sufficient {
        Vec::new()
    } else {
        lens_deficits(panel, &report)?
    };
    let bound = SufficiencyBound {
        i_panel_oracle: report_bits.sufficiency_basis,
        anchor_entropy_bits: report_bits.anchor_entropy,
        dpi_ceiling: report_bits.sufficiency_basis,
        dpi_ceiling_unit: UnitInterval::from_bits_ratio(
            report_bits.sufficiency_basis,
            report_bits.anchor_entropy,
        )
        .ok_or_else(invalid_report_error)?,
        sufficient,
        per_sensor_deficit,
    };

    if sufficient {
        Ok(bound)
    } else {
        Err(OracleError::Insufficient { bound })
    }
}

pub(crate) fn check_sufficiency_with_store(
    store: &AssayStore,
    vault_id: VaultId,
    panel: &Panel,
    domain: DomainId,
    clock: &dyn Clock,
) -> Result<SufficiencyBound, OracleError> {
    let assay = StoreSufficiencyAssay { store, vault_id };
    check_sufficiency_with_assay(&assay, panel, domain, clock)
}

pub struct VaultSufficiencyAssay<'a, C>
where
    C: Clock,
{
    vault: &'a AsterVault<C>,
}

impl<'a, C> VaultSufficiencyAssay<'a, C>
where
    C: Clock,
{
    pub const fn new(vault: &'a AsterVault<C>) -> Self {
        Self { vault }
    }
}

struct StoreSufficiencyAssay<'a> {
    store: &'a AssayStore,
    vault_id: VaultId,
}

impl SufficiencyAssay for StoreSufficiencyAssay<'_> {
    fn panel_sufficiency(
        &self,
        panel: &Panel,
        domain: &DomainId,
        clock: &dyn Clock,
    ) -> Result<PanelSufficiency, OracleError> {
        panel_sufficiency_from_store(self.store, self.vault_id, panel, domain, clock)
    }
}

impl<C> SufficiencyAssay for VaultSufficiencyAssay<'_, C>
where
    C: Clock,
{
    fn panel_sufficiency(
        &self,
        panel: &Panel,
        domain: &DomainId,
        clock: &dyn Clock,
    ) -> Result<PanelSufficiency, OracleError> {
        let store = AssayStore::load_from_vault(self.vault).map_err(OracleError::from)?;
        panel_sufficiency_from_store(&store, self.vault.vault_id(), panel, domain, clock)
    }
}

fn panel_sufficiency_from_store(
    store: &AssayStore,
    vault_id: VaultId,
    panel: &Panel,
    domain: &DomainId,
    clock: &dyn Clock,
) -> Result<PanelSufficiency, OracleError> {
    let key = AssayCacheKey::scoped(panel.version, domain.as_str(), vault_id, AnchorKind::Reward);
    let panel_row = required_row(store, &key, &AssaySubject::Panel)?;
    require_instrument(
        panel_row,
        &[EstimatorKind::PanelSufficiency],
        "synapse-assay-sufficiency",
        "panel sufficiency",
    )?;
    let panel_estimate = &panel_row.estimate;
    let outcome_row = required_row(store, &key, &AssaySubject::OutcomeEntropy)?;
    require_instrument(
        outcome_row,
        &[EstimatorKind::OutcomeEntropy],
        "synapse-assay-sufficiency",
        "outcome entropy",
    )?;
    let outcome_entropy_bits = bits(outcome_row, "outcome entropy")?;
    let slot_bits = panel
        .slots
        .iter()
        .map(|slot| {
            let row = required_row(store, &key, &AssaySubject::Lens { slot: slot.slot_id })?;
            require_instrument(
                row,
                &[EstimatorKind::Ksg, EstimatorKind::DiscretePlugin],
                "synapse-assay-bits",
                "lens",
            )?;
            Ok((slot.slot_id, bits(row, "lens")?))
        })
        .collect::<Result<Vec<_>, OracleError>>()?;

    let attributions = per_sensor_attribution(&slot_bits, SOLE_CARRIER_BITS);
    panel_sufficiency_from_estimate_with_context(
        panel_estimate,
        outcome_entropy_bits,
        &attributions,
        trust(store, &key),
        DeficitRoutingContext {
            panel_id: format!("oracle:{domain}:panel:{}", panel.version),
            anchor: AnchorKind::Reward,
            computed_at_seq: clock.now(),
            observation_scope: None,
        },
    )
    .map_err(OracleError::from)
}

fn require_instrument(
    row: &AssayRow,
    allowed_estimators: &[EstimatorKind],
    expected_provenance: &str,
    label: &str,
) -> Result<(), OracleError> {
    if allowed_estimators.contains(&row.estimate.estimator) && row.provenance == expected_provenance
    {
        return Ok(());
    }
    Err(CalyxError::aster_corrupt_shard(format!(
        "oracle sufficiency {label} row has estimator={:?} provenance={:?}; expected estimator in {allowed_estimators:?} and provenance={expected_provenance:?}",
        row.estimate.estimator, row.provenance
    ))
    .into())
}

fn required_row<'a>(
    store: &'a AssayStore,
    key: &AssayCacheKey,
    subject: &AssaySubject,
) -> Result<&'a AssayRow, OracleError> {
    store.get(key, subject).ok_or_else(|| {
        CalyxError::assay_insufficient_samples(format!(
            "missing oracle sufficiency assay row for subject {subject:?}"
        ))
        .into()
    })
}

fn bits(row: &AssayRow, label: &str) -> Result<f32, OracleError> {
    let bits = row.estimate.bits;
    if bits.is_finite() && bits >= 0.0 {
        Ok(bits)
    } else {
        Err(CalyxError::aster_corrupt_shard(format!(
            "oracle sufficiency {label} bits must be finite and non-negative"
        ))
        .into())
    }
}

fn trust(store: &AssayStore, key: &AssayCacheKey) -> TrustTag {
    store
        .get(key, &AssaySubject::Panel)
        .map(|row| row.estimate.trust)
        .unwrap_or(TrustTag::Provisional)
}

fn validate_report(report: &PanelSufficiency) -> Result<ReportBits, OracleError> {
    let _panel_bits = Bits::nonnegative(report.panel_bits).ok_or_else(invalid_report_error)?;
    let sufficiency_basis =
        Bits::nonnegative(report.sufficiency_basis_bits).ok_or_else(invalid_report_error)?;
    let anchor_entropy =
        Bits::positive(report.anchor_entropy_bits).ok_or_else(invalid_report_error)?;
    Ok(ReportBits {
        sufficiency_basis,
        anchor_entropy,
    })
}

fn invalid_report_error() -> OracleError {
    CalyxError::assay_insufficient_samples(
        "oracle sufficiency report requires finite non-negative panel bits and positive anchor entropy bits",
    )
    .into()
}

#[derive(Clone, Copy, Debug)]
struct ReportBits {
    sufficiency_basis: Bits,
    anchor_entropy: Bits,
}

fn lens_deficits(
    panel: &Panel,
    report: &PanelSufficiency,
) -> Result<Vec<(LensId, f32)>, OracleError> {
    if panel.slots.is_empty() {
        return Ok(Vec::new());
    }

    let mut per_slot = BTreeMap::<SlotId, f32>::new();
    for deficit in &report.deficits {
        if let Some(slot) = deficit.slot {
            per_slot.entry(slot).or_insert(deficit.deficit_bits);
        }
        for (slot, gap) in &deficit.per_slot_gaps {
            per_slot.insert(*slot, *gap);
        }
    }

    let mut by_lens = BTreeMap::<LensId, f32>::new();
    for slot in &panel.slots {
        if let Some(gap) = per_slot.get(&slot.slot_id).copied()
            && gap.is_finite()
            && gap > 0.0
        {
            *by_lens.entry(slot.lens_id).or_default() += gap;
        }
    }

    if by_lens.is_empty() {
        return Err(CalyxError::assay_insufficient_samples(
            "oracle insufficiency lacks per-sensor deficit attribution",
        )
        .into());
    }
    Ok(by_lens.into_iter().collect())
}
