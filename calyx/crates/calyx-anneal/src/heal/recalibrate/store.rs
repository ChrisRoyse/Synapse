use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use calyx_core::{PanelSlotId, Result};
use serde::{Deserialize, Serialize};

use super::types::{
    NewTau, WARD_TAU_TAG, WardTauReadback, WardTauStore, invalid_tau, validate_tau,
};
use crate::LogicalTime;

const UNMEASURED_ERROR_RATE: f64 = 1.0;

pub struct FileWardTauStore {
    path: PathBuf,
    rows: BTreeMap<PanelSlotId, WardTauReadback>,
}

impl FileWardTauStore {
    pub fn open(vault: impl AsRef<Path>) -> Result<Self> {
        let path = ward_tau_path(vault.as_ref());
        if !path.exists() {
            return Ok(Self {
                path,
                rows: BTreeMap::new(),
            });
        }
        let bytes = fs::read(&path)
            .map_err(|error| invalid_tau(format!("read {}: {error}", path.display())))?;
        let file = serde_json::from_slice::<WardTauFile>(&bytes)
            .map_err(|error| invalid_tau(format!("decode {}: {error}", path.display())))?;
        if file.tag != WARD_TAU_TAG {
            return Err(invalid_tau("ward tau file tag mismatch"));
        }
        let mut rows = BTreeMap::new();
        for row in file.slots {
            validate_tau(row.tau)?;
            rows.insert(row.panel_slot, row);
        }
        Ok(Self { path, rows })
    }

    pub fn upsert_current(
        &mut self,
        panel_slot: PanelSlotId,
        tau: f32,
        updated_at: LogicalTime,
    ) -> Result<()> {
        validate_tau(tau)?;
        self.rows.insert(
            panel_slot,
            WardTauReadback {
                panel_slot,
                tau,
                far: self
                    .rows
                    .get(&panel_slot)
                    .map_or(UNMEASURED_ERROR_RATE, |row| row.far),
                frr: self
                    .rows
                    .get(&panel_slot)
                    .map_or(UNMEASURED_ERROR_RATE, |row| row.frr),
                updated_at,
            },
        );
        self.persist()
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn persist(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| invalid_tau(format!("create {}: {error}", parent.display())))?;
        }
        let file = WardTauFile {
            tag: WARD_TAU_TAG.to_string(),
            slots: self.rows.values().cloned().collect(),
        };
        let bytes = serde_json::to_vec_pretty(&file)
            .map_err(|error| invalid_tau(format!("encode ward tau file: {error}")))?;
        atomic_write(&self.path, &bytes)
    }
}

impl WardTauStore for FileWardTauStore {
    fn current_tau(&self, panel_slot: PanelSlotId) -> Result<Option<f32>> {
        Ok(self.rows.get(&panel_slot).map(|row| row.tau))
    }

    fn set_live_tau(
        &mut self,
        panel_slot: PanelSlotId,
        tau: &NewTau,
        updated_at: LogicalTime,
    ) -> Result<()> {
        if tau.panel_slot != panel_slot {
            return Err(invalid_tau(
                "new tau panel_slot does not match target panel slot",
            ));
        }
        self.rows.insert(
            panel_slot,
            WardTauReadback {
                panel_slot,
                tau: tau.tau,
                far: tau.far,
                frr: tau.frr,
                updated_at,
            },
        );
        self.persist()
    }

    fn readback(&self) -> Result<Vec<WardTauReadback>> {
        Ok(self.rows.values().cloned().collect())
    }
}

#[derive(Serialize, Deserialize)]
struct WardTauFile {
    tag: String,
    slots: Vec<WardTauReadback>,
}

pub fn ward_tau_path(vault: &Path) -> PathBuf {
    vault.join(".anneal").join("ward_tau.json")
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = temp_path(path)?;
    fs::write(&tmp, bytes)
        .map_err(|error| invalid_tau(format!("write {}: {error}", tmp.display())))?;
    fs::rename(&tmp, path).map_err(|error| {
        let _ = fs::remove_file(&tmp);
        invalid_tau(format!(
            "rename {} -> {}: {error}",
            tmp.display(),
            path.display()
        ))
    })
}

fn temp_path(path: &Path) -> Result<PathBuf> {
    let file_name = path
        .file_name()
        .ok_or_else(|| invalid_tau("ward tau path must include a file name"))?;
    let mut tmp_name = file_name.to_os_string();
    tmp_name.push(format!(".tmp-{}", std::process::id()));
    Ok(path.with_file_name(tmp_name))
}
