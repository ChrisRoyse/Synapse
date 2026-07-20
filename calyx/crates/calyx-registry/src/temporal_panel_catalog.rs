use calyx_aster::cf::{ColumnFamily, KeyRange};
use calyx_aster::vault::AsterVault;
use calyx_core::{CalyxError, Clock, Result, SlotShape, TemporalPolicy};
use serde::{Deserialize, Serialize};

use crate::panels::{AlgorithmicPanelLens, PanelLensRuntime, PanelTemplate};

pub const CALYX_TEMPORAL_PANEL_INVALID: &str = "CALYX_TEMPORAL_PANEL_INVALID";
pub const CALYX_TEMPORAL_PANEL_CONFLICT: &str = "CALYX_TEMPORAL_PANEL_CONFLICT";
pub const CALYX_TEMPORAL_PANEL_MISSING: &str = "CALYX_TEMPORAL_PANEL_MISSING";

const CATALOG_SCHEMA_VERSION: u16 = 1;
const KEY_PREFIX: &[u8] = b"temporal-panel\0";
const MAX_PANEL_NAME_BYTES: usize = 128;

/// Durable registration of Calyx's retrieval-only temporal sidecars for one
/// exact source-panel generation. The source panel remains the owner of Base
/// and content slots; these specs are query-time-only and never become primary
/// recall or deduplication inputs.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VaultTemporalPanelRegistration {
    pub schema_version: u16,
    pub source_panel_version: u32,
    pub template: PanelTemplate,
    pub policy: TemporalPolicy,
    pub registered_at_unix_ms: u64,
}

impl VaultTemporalPanelRegistration {
    pub fn new(
        source_panel_version: u32,
        panel_name: impl Into<String>,
        policy: TemporalPolicy,
        registered_at_unix_ms: u64,
    ) -> Result<Self> {
        let registration = Self {
            schema_version: CATALOG_SCHEMA_VERSION,
            source_panel_version,
            template: PanelTemplate {
                name: panel_name.into(),
                slots: vec![
                    crate::panels::PanelSlotSpec::temporal(
                        "E2_recency",
                        AlgorithmicPanelLens::TemporalRecent,
                        SlotShape::Dense(1),
                    ),
                    crate::panels::PanelSlotSpec::temporal(
                        "E3_periodic",
                        AlgorithmicPanelLens::TemporalPeriodic,
                        SlotShape::Dense(2),
                    ),
                    crate::panels::PanelSlotSpec::temporal(
                        "E4_positional",
                        AlgorithmicPanelLens::TemporalPositional,
                        SlotShape::Dense(4),
                    ),
                ],
            },
            policy,
            registered_at_unix_ms,
        };
        registration.validate()?;
        Ok(registration)
    }

    pub fn validate(&self) -> Result<()> {
        if self.schema_version != CATALOG_SCHEMA_VERSION {
            return Err(invalid(format!(
                "temporal panel registration schema {} != {CATALOG_SCHEMA_VERSION}",
                self.schema_version
            )));
        }
        validate_panel_name(&self.template.name)?;
        if self.source_panel_version == 0 {
            return Err(invalid("source_panel_version must be non-zero"));
        }
        if self.registered_at_unix_ms == 0 {
            return Err(invalid("registered_at_unix_ms must be non-zero"));
        }
        self.policy.validate()?;
        if !self.policy.enabled || !self.policy.never_dominant {
            return Err(invalid(
                "temporal panel policy must be enabled and never_dominant",
            ));
        }
        if self.policy.recurrence_boost.is_some() {
            return Err(invalid(
                "event-panel registration cannot claim recurrence boost; recurrence series are keyed by stable subject CxIds",
            ));
        }
        let expected = [
            (
                "E2_recency",
                AlgorithmicPanelLens::TemporalRecent,
                SlotShape::Dense(1),
            ),
            (
                "E3_periodic",
                AlgorithmicPanelLens::TemporalPeriodic,
                SlotShape::Dense(2),
            ),
            (
                "E4_positional",
                AlgorithmicPanelLens::TemporalPositional,
                SlotShape::Dense(4),
            ),
        ];
        if self.template.slots.len() != expected.len() {
            return Err(invalid(format!(
                "temporal panel {} has {} sidecars; expected exactly {}",
                self.template.name,
                self.template.slots.len(),
                expected.len()
            )));
        }
        for (index, (slot, (name, lens, shape))) in
            self.template.slots.iter().zip(expected).enumerate()
        {
            if slot.name != name
                || slot.runtime != (PanelLensRuntime::Algorithmic { lens })
                || slot.output != shape
                || !slot.retrieval_only
                || !slot.excluded_from_dedup
                || slot.required
            {
                return Err(invalid(format!(
                    "temporal panel {} sidecar index {index} violates the canonical {name} retrieval-only contract",
                    self.template.name
                )));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TemporalPanelRegistrationDisposition {
    Inserted,
    ExistingIdentical,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VaultTemporalPanelRegistrationWrite {
    pub disposition: TemporalPanelRegistrationDisposition,
    pub committed_seq: u64,
    pub registration: VaultTemporalPanelRegistration,
}

pub fn register_vault_temporal_panel<C: Clock>(
    vault: &AsterVault<C>,
    registration: &VaultTemporalPanelRegistration,
) -> Result<VaultTemporalPanelRegistrationWrite> {
    registration.validate()?;
    let key = catalog_key(
        &registration.template.name,
        registration.source_panel_version,
    )?;
    let encoded = serde_json::to_vec(registration).map_err(|error| {
        invalid(format!(
            "encode temporal panel {} generation {}: {error}",
            registration.template.name, registration.source_panel_version
        ))
    })?;
    if let Some(existing) = vault.read_cf_latest(ColumnFamily::Registry, &key)? {
        let existing = decode_registration(&existing, &key)?;
        if !same_contract(&existing, registration) {
            return Err(CalyxError {
                code: CALYX_TEMPORAL_PANEL_CONFLICT,
                message: format!(
                    "temporal panel {} generation {} is already registered with different immutable bytes",
                    registration.template.name, registration.source_panel_version
                ),
                remediation: "publish a new source panel generation; never reinterpret an existing panel version",
            });
        }
        return Ok(VaultTemporalPanelRegistrationWrite {
            disposition: TemporalPanelRegistrationDisposition::ExistingIdentical,
            committed_seq: vault.latest_seq(),
            registration: existing,
        });
    }
    let committed_seq = vault.write_cf(ColumnFamily::Registry, key.clone(), encoded)?;
    let readback = vault
        .read_cf_at(committed_seq, ColumnFamily::Registry, &key)?
        .ok_or_else(|| CalyxError {
            code: CALYX_TEMPORAL_PANEL_MISSING,
            message: format!(
                "temporal panel {} generation {} missing at committed sequence {committed_seq}",
                registration.template.name, registration.source_panel_version
            ),
            remediation: "inspect the Registry CF and durable commit path before retrying registration",
        })?;
    let readback = decode_registration(&readback, &key)?;
    if readback != *registration {
        return Err(CalyxError {
            code: CALYX_TEMPORAL_PANEL_CONFLICT,
            message: format!(
                "temporal panel {} generation {} readback differs from the committed registration",
                registration.template.name, registration.source_panel_version
            ),
            remediation: "stop the writer and inspect Registry CF durability before serving temporal queries",
        });
    }
    Ok(VaultTemporalPanelRegistrationWrite {
        disposition: TemporalPanelRegistrationDisposition::Inserted,
        committed_seq,
        registration: readback,
    })
}

fn same_contract(
    left: &VaultTemporalPanelRegistration,
    right: &VaultTemporalPanelRegistration,
) -> bool {
    left.schema_version == right.schema_version
        && left.source_panel_version == right.source_panel_version
        && left.template == right.template
        && left.policy == right.policy
}

pub fn read_vault_temporal_panel<C: Clock>(
    vault: &AsterVault<C>,
    panel_name: &str,
    source_panel_version: u32,
) -> Result<Option<VaultTemporalPanelRegistration>> {
    let key = catalog_key(panel_name, source_panel_version)?;
    vault
        .read_cf_latest(ColumnFamily::Registry, &key)?
        .map(|bytes| decode_registration(&bytes, &key))
        .transpose()
}

pub fn list_vault_temporal_panels<C: Clock>(
    vault: &AsterVault<C>,
) -> Result<Vec<VaultTemporalPanelRegistration>> {
    let end = prefix_end(KEY_PREFIX).ok_or_else(|| invalid("temporal panel prefix has no end"))?;
    let range = KeyRange {
        start: KEY_PREFIX.to_vec(),
        end: Some(end),
    };
    let rows = vault.scan_cf_range_latest(ColumnFamily::Registry, &range)?;
    let mut registrations = Vec::with_capacity(rows.len());
    for (key, value) in rows {
        let registration = decode_registration(&value, &key)?;
        let expected_key = catalog_key(
            &registration.template.name,
            registration.source_panel_version,
        )?;
        if expected_key != key {
            return Err(invalid(format!(
                "temporal panel Registry key {} does not match decoded identity {} generation {}",
                hex(&key),
                registration.template.name,
                registration.source_panel_version
            )));
        }
        registrations.push(registration);
    }
    registrations.sort_by(|left, right| {
        (&left.template.name, left.source_panel_version)
            .cmp(&(&right.template.name, right.source_panel_version))
    });
    Ok(registrations)
}

fn catalog_key(panel_name: &str, source_panel_version: u32) -> Result<Vec<u8>> {
    validate_panel_name(panel_name)?;
    if source_panel_version == 0 {
        return Err(invalid("source_panel_version must be non-zero"));
    }
    let mut key = Vec::with_capacity(KEY_PREFIX.len() + 2 + panel_name.len() + 4);
    key.extend_from_slice(KEY_PREFIX);
    key.extend_from_slice(&(panel_name.len() as u16).to_be_bytes());
    key.extend_from_slice(panel_name.as_bytes());
    key.extend_from_slice(&source_panel_version.to_be_bytes());
    Ok(key)
}

fn validate_panel_name(panel_name: &str) -> Result<()> {
    if panel_name.is_empty()
        || panel_name.len() > MAX_PANEL_NAME_BYTES
        || !panel_name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Err(invalid(format!(
            "panel name {panel_name:?} must be 1..={MAX_PANEL_NAME_BYTES} lowercase ASCII letters, digits, or hyphens"
        )));
    }
    Ok(())
}

fn decode_registration(bytes: &[u8], key: &[u8]) -> Result<VaultTemporalPanelRegistration> {
    let registration: VaultTemporalPanelRegistration =
        serde_json::from_slice(bytes).map_err(|error| {
            invalid(format!(
                "decode temporal panel Registry row {}: {error}",
                hex(key)
            ))
        })?;
    registration.validate()?;
    Ok(registration)
}

fn prefix_end(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut end = prefix.to_vec();
    for index in (0..end.len()).rev() {
        if end[index] != u8::MAX {
            end[index] += 1;
            end.truncate(index + 1);
            return Some(end);
        }
    }
    None
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn invalid(message: impl Into<String>) -> CalyxError {
    CalyxError {
        code: CALYX_TEMPORAL_PANEL_INVALID,
        message: message.into(),
        remediation: "repair the exact panel registration and publish a new generation for incompatible changes",
    }
}
