//! In-memory Assay result CF/cache with provenance.

use std::collections::BTreeMap;

use calyx_aster::cf::{CfRouter, ColumnFamily};
use calyx_aster::{mvcc::Snapshot, vault::AsterVault};
use calyx_core::{AnchorKind, CalyxError, Clock, Result, SlotId, VaultId, VaultStore};
use serde::{Deserialize, Serialize};

use crate::estimate::MiEstimate;

type AsterAssayRow = (ColumnFamily, Vec<u8>, Vec<u8>);
const ASSAY_SCAN_PAGE_ROWS: usize = 1024;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct AssayCacheKey {
    #[serde(default)]
    pub vault_id: Option<VaultId>,
    #[serde(default = "default_anchor")]
    pub anchor: AnchorKind,
    pub panel_version: u32,
    pub corpus_shard: String,
}

impl AssayCacheKey {
    /// Compatibility constructor for legacy tests and unscoped probes.
    #[deprecated(note = "Assay CF rows must use AssayCacheKey::scoped before persistence")]
    pub fn new(panel_version: u32, corpus_shard: impl Into<String>) -> Self {
        Self {
            vault_id: None,
            anchor: default_anchor(),
            panel_version,
            corpus_shard: corpus_shard.into(),
        }
    }

    pub fn scoped(
        panel_version: u32,
        corpus_shard: impl Into<String>,
        vault_id: VaultId,
        anchor: AnchorKind,
    ) -> Self {
        Self {
            vault_id: Some(vault_id),
            anchor,
            panel_version,
            corpus_shard: corpus_shard.into(),
        }
    }

    pub fn require_scoped(&self) -> Result<()> {
        if self.vault_id.is_none() {
            return Err(CalyxError::vault_access_denied(
                "assay cache key must include explicit vault scope",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AssaySubject {
    Lens { slot: SlotId },
    Pair { a: SlotId, b: SlotId },
    Panel,
    OutcomeEntropy,
    EnsembleCard,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AssayRow {
    pub cache_key: AssayCacheKey,
    pub subject: AssaySubject,
    pub estimate: MiEstimate,
    pub provenance: String,
    pub written_at_seq: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<serde_json::Value>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AssayStore {
    rows: BTreeMap<(AssayCacheKey, AssaySubject), AssayRow>,
}

impl AssayStore {
    pub fn put(
        &mut self,
        cache_key: AssayCacheKey,
        subject: AssaySubject,
        estimate: MiEstimate,
        provenance: impl Into<String>,
        written_at_seq: u64,
    ) {
        let row = AssayRow {
            cache_key: cache_key.clone(),
            subject: subject.clone(),
            estimate,
            provenance: provenance.into(),
            written_at_seq,
            payload: None,
        };
        self.rows.insert((cache_key, subject), row);
    }

    pub fn put_with_payload(
        &mut self,
        cache_key: AssayCacheKey,
        subject: AssaySubject,
        estimate: MiEstimate,
        provenance: impl Into<String>,
        written_at_seq: u64,
        payload: serde_json::Value,
    ) {
        let row = AssayRow {
            cache_key: cache_key.clone(),
            subject: subject.clone(),
            estimate,
            provenance: provenance.into(),
            written_at_seq,
            payload: Some(payload),
        };
        self.rows.insert((cache_key, subject), row);
    }

    pub fn get(&self, cache_key: &AssayCacheKey, subject: &AssaySubject) -> Option<&AssayRow> {
        self.rows.get(&(cache_key.clone(), subject.clone()))
    }

    pub fn cache_hit(&self, cache_key: &AssayCacheKey, subject: &AssaySubject) -> bool {
        self.get(cache_key, subject).is_some()
    }

    pub fn invalidate_panel(&mut self, panel_version: u32) -> usize {
        let before = self.rows.len();
        self.rows
            .retain(|(key, _), _| key.panel_version != panel_version);
        before - self.rows.len()
    }

    pub fn rows(&self) -> Vec<AssayRow> {
        self.rows.values().cloned().collect()
    }

    pub fn persist_to_aster(&self, router: &mut CfRouter) -> Result<usize> {
        for (_, key, value) in self.aster_rows()? {
            router.put(ColumnFamily::Assay, &key, &value)?;
        }
        router.flush_cf(ColumnFamily::Assay)?;
        Ok(self.rows.len())
    }

    pub fn persist_to_vault<C>(&self, vault: &AsterVault<C>) -> Result<usize>
    where
        C: Clock,
    {
        vault.write_cf_batch(self.aster_rows()?)?;
        vault.flush()?;
        Ok(self.rows.len())
    }

    pub fn load_from_aster(router: &CfRouter) -> Result<Self> {
        let mut store = Self::default();
        for entry in router.iter_cf(ColumnFamily::Assay)? {
            store.insert_aster_row(entry.key, entry.value)?;
        }
        Ok(store)
    }

    pub fn load_from_vault<C: Clock>(vault: &AsterVault<C>) -> Result<Self> {
        Self::load_from_vault_at(vault, vault.snapshot())
    }

    pub fn load_from_vault_at<C: Clock>(vault: &AsterVault<C>, snapshot: u64) -> Result<Self> {
        let mut store = Self::default();
        vault.scan_cf_pages_at(
            snapshot,
            ColumnFamily::Assay,
            ASSAY_SCAN_PAGE_ROWS,
            |rows| {
                for (key, value) in rows {
                    store.insert_aster_row(key, value)?;
                }
                Ok::<(), CalyxError>(())
            },
        )?;
        Ok(store)
    }

    /// Loads every Assay row from one caller-owned MVCC snapshot.
    ///
    /// This is the composition boundary for a larger cross-column-family read:
    /// the caller owns the lease lifetime and every Assay, Base, and recurrence
    /// read can therefore observe the same committed sequence.
    pub fn load_from_vault_snapshot<C: Clock>(
        vault: &AsterVault<C>,
        snapshot: Snapshot,
    ) -> Result<Self> {
        let mut store = Self::default();
        vault.scan_cf_pages_snapshot(
            snapshot,
            ColumnFamily::Assay,
            ASSAY_SCAN_PAGE_ROWS,
            |rows| {
                for (key, value) in rows {
                    store.insert_aster_row(key, value)?;
                }
                Ok::<(), CalyxError>(())
            },
        )?;
        Ok(store)
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    fn aster_rows(&self) -> Result<Vec<AsterAssayRow>> {
        let mut out = Vec::with_capacity(self.rows.len());
        for row in self.rows.values() {
            row.cache_key.require_scoped()?;
            let key = assay_key(&row.cache_key, &row.subject);
            let value = serde_json::to_vec(row)
                .map_err(|error| CalyxError::disk_pressure(format!("encode assay row: {error}")))?;
            out.push((ColumnFamily::Assay, key, value));
        }
        Ok(out)
    }

    fn insert_aster_row(&mut self, key: Vec<u8>, value: Vec<u8>) -> Result<()> {
        let row: AssayRow = serde_json::from_slice(&value).map_err(|error| {
            CalyxError::aster_corrupt_shard(format!("decode assay row: {error}"))
        })?;
        row.cache_key.require_scoped()?;
        let expected = assay_key(&row.cache_key, &row.subject);
        if key != expected {
            return Err(CalyxError::aster_corrupt_shard(
                "assay CF key does not match row subject",
            ));
        }
        self.rows
            .insert((row.cache_key.clone(), row.subject.clone()), row);
        Ok(())
    }
}

fn assay_key(cache_key: &AssayCacheKey, subject: &AssaySubject) -> Vec<u8> {
    let vault = cache_key
        .vault_id
        .expect("assay cache key scope validated before encoding")
        .to_string();
    let shard = cache_key.corpus_shard.as_bytes();
    let anchor = serde_json::to_vec(&cache_key.anchor).expect("anchor kind serializes");
    let mut key = Vec::with_capacity(48 + vault.len() + anchor.len() + shard.len());
    key.extend_from_slice(&cache_key.panel_version.to_be_bytes());
    push_len_prefixed(&mut key, vault.as_bytes());
    push_len_prefixed(&mut key, &anchor);
    push_len_prefixed(&mut key, shard);
    match subject {
        AssaySubject::Lens { slot } => {
            key.push(0);
            key.extend_from_slice(&slot.get().to_be_bytes());
        }
        AssaySubject::Pair { a, b } => {
            key.push(1);
            key.extend_from_slice(&a.get().to_be_bytes());
            key.extend_from_slice(&b.get().to_be_bytes());
        }
        AssaySubject::Panel => key.push(2),
        AssaySubject::OutcomeEntropy => key.push(3),
        AssaySubject::EnsembleCard => key.push(4),
    }
    key
}

fn push_len_prefixed(key: &mut Vec<u8>, value: &[u8]) {
    key.extend_from_slice(&(value.len() as u32).to_be_bytes());
    key.extend_from_slice(value);
}

fn default_anchor() -> AnchorKind {
    AnchorKind::Reward
}
