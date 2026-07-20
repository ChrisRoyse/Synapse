use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use calyx_aster::cf::ColumnFamily;
use calyx_aster::vault::AsterVault;
use calyx_core::{CalyxError, Clock, LedgerRef, LensId, PanelSlotId, Result, Seq};
use calyx_ledger::LedgerCfStore;
use serde::{Deserialize, Serialize};

use crate::{
    AnnealLedger, AnnealLedgerAction, AnnealLedgerEntry, CALYX_ASTER_CF_UNAVAILABLE, ChangeId,
    LogicalTime, MetricSnapshot,
};

pub const ANNEAL_HEALTH_TAG: &str = "anneal_health_v1";
pub const CALYX_ANNEAL_HEAL_CONFIRMATION_REQUIRED: &str = "CALYX_ANNEAL_HEAL_CONFIRMATION_REQUIRED";
pub const CALYX_ANNEAL_HEALTH_INVALID_ROW: &str = "CALYX_ANNEAL_HEALTH_INVALID_ROW";

static OK_HEALTH: ComponentHealth = ComponentHealth::Ok;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ScopeId(String);

impl ScopeId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn from_hash(hash: [u8; 32]) -> Self {
        Self(hex_bytes(&hash))
    }
}

impl fmt::Display for ScopeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ComponentHealth {
    Ok,
    Degraded { since: LogicalTime, reason: String },
    Failing { since: LogicalTime, reason: String },
    Parked { since: LogicalTime, reason: String },
}

impl ComponentHealth {
    pub fn degraded(since: LogicalTime, reason: impl Into<String>) -> Self {
        Self::Degraded {
            since,
            reason: reason.into(),
        }
    }

    pub fn failing(since: LogicalTime, reason: impl Into<String>) -> Self {
        Self::Failing {
            since,
            reason: reason.into(),
        }
    }

    pub fn parked(since: LogicalTime, reason: impl Into<String>) -> Self {
        Self::Parked {
            since,
            reason: reason.into(),
        }
    }

    pub const fn state_name(&self) -> &'static str {
        match self {
            Self::Ok => "Ok",
            Self::Degraded { .. } => "Degraded",
            Self::Failing { .. } => "Failing",
            Self::Parked { .. } => "Parked",
        }
    }

    pub const fn excludes_lens(&self) -> bool {
        matches!(self, Self::Failing { .. } | Self::Parked { .. })
    }
}

impl fmt::Display for ComponentHealth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ok => f.write_str("Ok"),
            Self::Degraded { since, reason } => {
                write!(f, "Degraded since={since} reason={reason}")
            }
            Self::Failing { since, reason } => {
                write!(f, "Failing since={since} reason={reason}")
            }
            Self::Parked { since, reason } => {
                write!(f, "Parked since={since} reason={reason}")
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComponentKind {
    AnnIndex { panel_slot: PanelSlotId },
    KernelIndex { scope: ScopeId },
    GuardProfile { panel_slot: PanelSlotId },
    LensEndpoint { lens_id: LensId },
    BaseShard { shard_id: String },
}

impl ComponentKind {
    pub fn ann_index(panel_slot: PanelSlotId) -> Self {
        Self::AnnIndex { panel_slot }
    }

    pub fn lens_endpoint(lens_id: LensId) -> Self {
        Self::LensEndpoint { lens_id }
    }

    pub fn base_shard(shard_id: impl Into<String>) -> Self {
        Self::BaseShard {
            shard_id: shard_id.into(),
        }
    }

    pub(crate) fn storage_key(&self) -> Vec<u8> {
        match self {
            Self::AnnIndex { panel_slot } => format!(
                "ann_index/panel_{:010}/slot_{:04}",
                panel_slot.panel_version(),
                panel_slot.slot_id().get()
            ),
            Self::KernelIndex { scope } => format!("kernel_index/{scope}"),
            Self::GuardProfile { panel_slot } => format!(
                "guard_profile/panel_{:010}/slot_{:04}",
                panel_slot.panel_version(),
                panel_slot.slot_id().get()
            ),
            Self::LensEndpoint { lens_id } => format!("lens_endpoint/{lens_id}"),
            Self::BaseShard { shard_id } => format!("base_shard/{shard_id}"),
        }
        .into_bytes()
    }
}

impl fmt::Display for ComponentKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AnnIndex { panel_slot } => write!(f, "AnnIndex({panel_slot})"),
            Self::KernelIndex { scope } => write!(f, "KernelIndex({scope})"),
            Self::GuardProfile { panel_slot } => write!(f, "GuardProfile({panel_slot})"),
            Self::LensEndpoint { lens_id } => write!(f, "LensEndpoint({lens_id})"),
            Self::BaseShard { shard_id } => write!(f, "BaseShard({shard_id})"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthRowReadback {
    pub kind: ComponentKind,
    pub health: ComponentHealth,
    pub updated_at: LogicalTime,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LensRoute {
    pub requested: Vec<LensId>,
    pub active: Vec<LensId>,
    pub degraded: bool,
}

#[derive(Serialize, Deserialize)]
struct HealthRow {
    tag: String,
    kind: ComponentKind,
    health: ComponentHealth,
    updated_at: LogicalTime,
}

pub trait HealthStorage: Send + Sync {
    fn put(&self, key: Vec<u8>, value: Vec<u8>) -> Result<Seq>;
    fn scan(&self) -> Result<Vec<(Vec<u8>, Vec<u8>)>>;
}

pub struct AsterHealthStore<'a, C>
where
    C: Clock,
{
    vault: &'a AsterVault<C>,
}

impl<'a, C> AsterHealthStore<'a, C>
where
    C: Clock,
{
    pub const fn new(vault: &'a AsterVault<C>) -> Self {
        Self { vault }
    }
}

impl<C> HealthStorage for AsterHealthStore<'_, C>
where
    C: Clock,
{
    fn put(&self, key: Vec<u8>, value: Vec<u8>) -> Result<Seq> {
        self.vault
            .write_cf(ColumnFamily::AnnealHealth, key, value)
            .map_err(|error| cf_unavailable(format!("write anneal_health CF: {error}")))
    }

    fn scan(&self) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        self.vault
            .scan_cf_at(self.vault.latest_seq(), ColumnFamily::AnnealHealth)
            .map_err(|error| cf_unavailable(format!("scan anneal_health CF: {error}")))
    }
}

pub struct DegradeRegistry<S> {
    components: HashMap<ComponentKind, ComponentHealth>,
    clock: Arc<dyn Clock>,
    storage: S,
}

impl<S> DegradeRegistry<S>
where
    S: HealthStorage,
{
    pub fn open(clock: Arc<dyn Clock>, storage: S) -> Result<Self> {
        let mut components = HashMap::new();
        for (_key, value) in storage.scan()? {
            let row = decode_health_value(&value)?;
            components.insert(row.kind, row.health);
        }
        Ok(Self {
            components,
            clock,
            storage,
        })
    }

    pub fn set_health<L, C>(
        &mut self,
        kind: ComponentKind,
        health: ComponentHealth,
        ledger: &mut AnnealLedger<L, C>,
    ) -> Result<LedgerRef>
    where
        L: LedgerCfStore,
        C: Clock,
    {
        self.set_health_inner(kind, health, ledger, false)
    }

    pub fn confirm_healed<L, C>(
        &mut self,
        kind: ComponentKind,
        ledger: &mut AnnealLedger<L, C>,
    ) -> Result<LedgerRef>
    where
        L: LedgerCfStore,
        C: Clock,
    {
        self.set_health_inner(kind, ComponentHealth::Ok, ledger, true)
    }

    pub fn health(&self, kind: &ComponentKind) -> &ComponentHealth {
        self.components.get(kind).unwrap_or(&OK_HEALTH)
    }

    pub fn active_lenses(&self, all: &[LensId]) -> Vec<LensId> {
        all.iter()
            .copied()
            .filter(|lens_id| {
                let kind = ComponentKind::lens_endpoint(*lens_id);
                !self.health(&kind).excludes_lens()
            })
            .collect()
    }

    pub fn route_lens_panel(&self, panel: &[LensId]) -> LensRoute {
        let active = self.active_lenses(panel);
        LensRoute {
            requested: panel.to_vec(),
            degraded: active.len() != panel.len(),
            active,
        }
    }

    pub fn degraded_components(&self) -> Vec<(ComponentKind, ComponentHealth)> {
        let mut rows = self
            .components
            .iter()
            .filter(|(_, health)| !matches!(health, ComponentHealth::Ok))
            .map(|(kind, health)| (kind.clone(), health.clone()))
            .collect::<Vec<_>>();
        rows.sort_by(|left, right| left.0.cmp(&right.0));
        rows
    }

    pub fn components(&self) -> &HashMap<ComponentKind, ComponentHealth> {
        &self.components
    }

    fn set_health_inner<L, C>(
        &mut self,
        kind: ComponentKind,
        health: ComponentHealth,
        ledger: &mut AnnealLedger<L, C>,
        confirmed_heal: bool,
    ) -> Result<LedgerRef>
    where
        L: LedgerCfStore,
        C: Clock,
    {
        let prior = self.health(&kind).clone();
        if !confirmed_heal
            && !matches!(prior, ComponentHealth::Ok)
            && matches!(health, ComponentHealth::Ok)
        {
            return Err(heal_confirmation_required(&kind, &prior));
        }

        let row = encode_health_row(&kind, &health, self.clock.now())?;
        let ledger_ref = self.write_ledger_change(ledger, &kind, &prior, &health)?;
        self.storage.put(kind.storage_key(), row)?;
        self.components.insert(kind, health);
        Ok(ledger_ref)
    }

    fn write_ledger_change<L, C>(
        &self,
        ledger: &mut AnnealLedger<L, C>,
        kind: &ComponentKind,
        prior: &ComponentHealth,
        health: &ComponentHealth,
    ) -> Result<LedgerRef>
    where
        L: LedgerCfStore,
        C: Clock,
    {
        let ts = self.clock.now();
        let prior_bytes = encode_health_row(kind, prior, ts)?;
        let candidate_bytes = encode_health_row(kind, health, ts)?;
        ledger.write(AnnealLedgerEntry {
            action: AnnealLedgerAction::DegradeChange,
            change_id: change_id_for(kind, health, ts),
            artifact_id: hex_bytes(blake3::hash(&kind.storage_key()).as_bytes()),
            prior_ptr_hash: *blake3::hash(&prior_bytes).as_bytes(),
            candidate_ptr_hash: *blake3::hash(&candidate_bytes).as_bytes(),
            metrics: MetricSnapshot::empty(ts),
            ts,
            description: format!(
                "health transition {} to {}",
                prior.state_name(),
                health.state_name()
            ),
            fault: None,
            proposal: None,
            details: None,
            prev_hash: None,
        })
    }
}

pub fn decode_health_value(value: &[u8]) -> Result<HealthRowReadback> {
    let row = serde_json::from_slice::<HealthRow>(value)
        .map_err(|error| invalid_row(format!("decode anneal_health row: {error}")))?;
    if row.tag != ANNEAL_HEALTH_TAG {
        return Err(invalid_row("anneal_health row has invalid tag"));
    }
    Ok(HealthRowReadback {
        kind: row.kind,
        health: row.health,
        updated_at: row.updated_at,
    })
}

fn encode_health_row(
    kind: &ComponentKind,
    health: &ComponentHealth,
    updated_at: LogicalTime,
) -> Result<Vec<u8>> {
    serde_json::to_vec(&HealthRow {
        tag: ANNEAL_HEALTH_TAG.to_string(),
        kind: kind.clone(),
        health: health.clone(),
        updated_at,
    })
    .map_err(|error| invalid_row(format!("encode anneal_health row: {error}")))
}

fn change_id_for(kind: &ComponentKind, health: &ComponentHealth, ts: LogicalTime) -> ChangeId {
    let mut acc = ts ^ 0xcbf2_9ce4_8422_2325;
    for byte in kind
        .storage_key()
        .into_iter()
        .chain(health.state_name().bytes())
    {
        acc ^= byte as u64;
        acc = acc.wrapping_mul(0x1000_0000_01b3);
    }
    ChangeId(acc.max(1))
}

fn heal_confirmation_required(kind: &ComponentKind, prior: &ComponentHealth) -> CalyxError {
    CalyxError {
        code: CALYX_ANNEAL_HEAL_CONFIRMATION_REQUIRED,
        message: format!(
            "{kind} cannot transition {}->Ok without heal confirmation",
            prior.state_name()
        ),
        remediation: "call confirm_healed only after T03 rebuild verification succeeds",
    }
}

fn invalid_row(message: impl Into<String>) -> CalyxError {
    CalyxError {
        code: CALYX_ANNEAL_HEALTH_INVALID_ROW,
        message: message.into(),
        remediation: "repair or quarantine anneal_health CF rows before serving",
    }
}

fn cf_unavailable(message: impl Into<String>) -> CalyxError {
    CalyxError {
        code: CALYX_ASTER_CF_UNAVAILABLE,
        message: message.into(),
        remediation: "restore Aster anneal_health CF availability",
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
