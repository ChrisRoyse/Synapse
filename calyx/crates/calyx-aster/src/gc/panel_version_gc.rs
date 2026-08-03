//! PH58 panel/codebook version GC and retired-lens pruning.

mod codebook;

use crate::manifest::{ManifestStore, VaultManifest};
use crate::vault::AsterVault;
use crate::vault::encode::decode_constellation_base;
use calyx_core::{Clock, LensId, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

pub use codebook::{CodebookVersionGc, CodebookVersionGcTarget};

pub type PanelVersionId = u32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VersionTier {
    Hot,
    Cold,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PanelVersionRecord {
    pub id: PanelVersionId,
    pub tier: VersionTier,
    pub ledger_referenced: bool,
    pub bytes: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RetentionPolicy {
    pub hot_versions_to_keep: usize,
    pub cold_tier_first: bool,
    pub max_versions_per_run: usize,
}

impl Default for RetentionPolicy {
    fn default() -> Self {
        Self {
            hot_versions_to_keep: 2,
            cold_tier_first: true,
            max_versions_per_run: 1_000,
        }
    }
}

pub trait PanelVersionGcTarget {
    fn panel_versions(&self) -> Result<Vec<PanelVersionRecord>>;
    fn live_panel_versions(&self) -> Result<BTreeSet<PanelVersionId>>;
    fn move_panel_version_to_cold(&self, id: PanelVersionId) -> Result<u64>;
    fn purge_cold_panel_version(&self, id: PanelVersionId) -> Result<u64>;
}

pub trait RetiredLensGcTarget {
    fn retired_lens_bytes(&self, lens_id: LensId) -> Result<u64>;
    fn move_retired_lens_to_cold(&self, lens_id: LensId) -> Result<u64>;
    fn purge_retired_lens(&self, lens_id: LensId) -> Result<u64>;
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PanelVersionGcResult {
    pub moved_to_cold: usize,
    pub pruned: usize,
    pub skipped_ledger_referenced: usize,
    pub bytes_freed: u64,
    pub rate_limited: bool,
    pub panel_versions_pruned_total: u64,
    pub codebook_versions_pruned_total: u64,
    pub retired_lens_bytes_freed_total: u64,
}

impl PanelVersionGcResult {
    pub fn to_metrics_text(&self, vault_label: &str, live_versions: usize) -> String {
        let vault = escape_label(vault_label);
        let mut out = String::new();
        let _ = writeln!(
            out,
            "calyx_panel_versions_pruned_total{{vault=\"{vault}\"}} {}",
            self.panel_versions_pruned_total
        );
        let _ = writeln!(
            out,
            "calyx_panel_versions_live{{vault=\"{vault}\"}} {}",
            live_versions
        );
        let _ = writeln!(
            out,
            "calyx_codebook_versions_pruned_total{{vault=\"{vault}\"}} {}",
            self.codebook_versions_pruned_total
        );
        let _ = writeln!(
            out,
            "calyx_retired_lens_bytes_freed_total{{vault=\"{vault}\"}} {}",
            self.retired_lens_bytes_freed_total
        );
        out
    }
}

#[derive(Debug)]
pub struct PanelVersionGc {
    pub retention_policy: RetentionPolicy,
    panel_versions_pruned_total: AtomicU64,
}

impl PanelVersionGc {
    pub fn new(retention_policy: RetentionPolicy) -> Self {
        Self {
            retention_policy,
            panel_versions_pruned_total: AtomicU64::new(0),
        }
    }

    pub fn find_unreferenced<T>(&self, target: &T) -> Result<Vec<PanelVersionId>>
    where
        T: PanelVersionGcTarget + ?Sized,
    {
        let records = target.panel_versions()?;
        let live = target.live_panel_versions()?;
        let keep = latest_versions_to_keep(&records, self.retention_policy.hot_versions_to_keep);
        let mut ids = records
            .into_iter()
            .filter(|record| !live.contains(&record.id))
            .filter(|record| !keep.contains(&record.id))
            .map(|record| record.id)
            .collect::<Vec<_>>();
        ids.sort_unstable();
        ids.dedup();
        Ok(ids)
    }

    pub fn prune<T>(&self, target: &T, ids: &[PanelVersionId]) -> Result<PanelVersionGcResult>
    where
        T: PanelVersionGcTarget + ?Sized,
    {
        let records = target
            .panel_versions()?
            .into_iter()
            .map(|record| (record.id, record))
            .collect::<BTreeMap<_, _>>();
        let mut result = PanelVersionGcResult::default();
        for id in ids.iter().take(self.retention_policy.max_versions_per_run) {
            let Some(record) = records.get(id) else {
                continue;
            };
            if record.ledger_referenced {
                result.skipped_ledger_referenced += 1;
                continue;
            }
            if self.retention_policy.cold_tier_first && record.tier == VersionTier::Hot {
                target.move_panel_version_to_cold(*id)?;
                result.moved_to_cold += 1;
            } else {
                result.bytes_freed = result
                    .bytes_freed
                    .saturating_add(target.purge_cold_panel_version(*id)?);
                result.pruned += 1;
            }
        }
        result.rate_limited = ids.len() > self.retention_policy.max_versions_per_run;
        result.panel_versions_pruned_total = self
            .panel_versions_pruned_total
            .fetch_add(result.pruned as u64, Ordering::Relaxed)
            + result.pruned as u64;
        Ok(result)
    }
}

impl Default for PanelVersionGc {
    fn default() -> Self {
        Self::new(RetentionPolicy::default())
    }
}

#[derive(Debug)]
pub struct RetiredLensGc {
    pub retention_policy: RetentionPolicy,
    retired_lens_bytes_freed_total: AtomicU64,
}

impl RetiredLensGc {
    pub fn new(retention_policy: RetentionPolicy) -> Self {
        Self {
            retention_policy,
            retired_lens_bytes_freed_total: AtomicU64::new(0),
        }
    }

    pub fn prune_retired<T>(&self, target: &T, lens_id: LensId) -> Result<PanelVersionGcResult>
    where
        T: RetiredLensGcTarget + ?Sized,
    {
        let bytes = target.retired_lens_bytes(lens_id)?;
        let freed = if self.retention_policy.cold_tier_first {
            target.move_retired_lens_to_cold(lens_id)?
        } else {
            target.purge_retired_lens(lens_id)?
        };
        let total = self
            .retired_lens_bytes_freed_total
            .fetch_add(freed, Ordering::Relaxed)
            + freed;
        Ok(PanelVersionGcResult {
            moved_to_cold: usize::from(self.retention_policy.cold_tier_first && bytes > 0),
            pruned: usize::from(!self.retention_policy.cold_tier_first && freed > 0),
            bytes_freed: freed,
            retired_lens_bytes_freed_total: total,
            ..PanelVersionGcResult::default()
        })
    }
}

impl Default for RetiredLensGc {
    fn default() -> Self {
        Self::new(RetentionPolicy::default())
    }
}

pub struct VaultPanelVersionGcTarget<'a, C> {
    vault: &'a AsterVault<C>,
    hot_panel_dir: PathBuf,
    cold_panel_dir: PathBuf,
    hot_codebook_dir: PathBuf,
    cold_codebook_dir: PathBuf,
    manifest_panel_path: Option<String>,
    manifest_codebook_paths: BTreeSet<String>,
    protect_all_without_manifest: bool,
}

impl<'a, C> VaultPanelVersionGcTarget<'a, C> {
    /// Loads and verifies the current manifest before enabling GC mutations.
    /// A verified manifest-free vault is allowed but protects every version.
    pub fn new(
        vault: &'a AsterVault<C>,
        vault_dir: impl AsRef<Path>,
        cold_root: impl AsRef<Path>,
    ) -> Result<Self> {
        let vault_dir = vault_dir.as_ref();
        let manifest = load_gc_manifest(vault_dir)?;
        let protect_all_without_manifest = manifest.is_none();
        let (manifest_panel_path, manifest_codebook_paths) = manifest.map_or_else(
            || (None, BTreeSet::new()),
            |manifest| {
                (
                    Some(manifest.panel_ref.logical_path),
                    manifest
                        .codebook_refs
                        .into_iter()
                        .map(|reference| reference.logical_path)
                        .collect(),
                )
            },
        );
        Ok(Self {
            vault,
            hot_panel_dir: vault_dir.join("panel"),
            cold_panel_dir: cold_root.as_ref().join("panel"),
            hot_codebook_dir: vault_dir.join("codebooks"),
            cold_codebook_dir: cold_root.as_ref().join("codebooks"),
            manifest_panel_path,
            manifest_codebook_paths,
            protect_all_without_manifest,
        })
    }
}

fn load_gc_manifest(vault_dir: &Path) -> Result<Option<VaultManifest>> {
    match ManifestStore::open(vault_dir).load_current() {
        Ok(manifest) => Ok(Some(manifest)),
        Err(error) => {
            if manifest_artifacts_absent(vault_dir)? {
                Ok(None)
            } else {
                Err(error)
            }
        }
    }
}

fn manifest_artifacts_absent(vault_dir: &Path) -> Result<bool> {
    for name in ["CURRENT", "CURRENT.tmp", "MANIFEST", "MANIFEST.tmp"] {
        match fs::symlink_metadata(vault_dir.join(name)) {
            Ok(_) => return Ok(false),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(calyx_core::CalyxError::disk_pressure(format!(
                    "inspect {name} before panel GC: {error}"
                )));
            }
        }
    }
    let entries = match fs::read_dir(vault_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(true),
        Err(error) => {
            return Err(calyx_core::CalyxError::disk_pressure(format!(
                "inspect manifest directory before panel GC: {error}"
            )));
        }
    };
    for entry in entries {
        let entry = entry.map_err(|error| {
            calyx_core::CalyxError::disk_pressure(format!(
                "inspect manifest entry before panel GC: {error}"
            ))
        })?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("manifest-") && (name.ends_with(".json") || name.ends_with(".tmp")) {
            return Ok(false);
        }
    }
    Ok(true)
}

impl<C> PanelVersionGcTarget for VaultPanelVersionGcTarget<'_, C>
where
    C: Clock,
{
    fn panel_versions(&self) -> Result<Vec<PanelVersionRecord>> {
        let mut records = Vec::new();
        collect_panel_records(
            &self.hot_panel_dir,
            VersionTier::Hot,
            self.manifest_panel_path.as_deref(),
            self.protect_all_without_manifest,
            &mut records,
        )?;
        collect_panel_records(
            &self.cold_panel_dir,
            VersionTier::Cold,
            None,
            self.protect_all_without_manifest,
            &mut records,
        )?;
        records.sort_by_key(|record| (record.id, record.tier == VersionTier::Cold));
        Ok(records)
    }

    /// The panel versions at least one live `Base` row still belongs to.
    ///
    /// Walked in bounded pages and **atomically** (#1977). This set decides
    /// which panel/codebook versions may be moved to cold storage or deleted,
    /// so it is a destructive decision and must be taken against one instant —
    /// a version that gains its first row mid-walk must not be GC'd as unused.
    /// `walk_cf_pages_atomic` fails closed if the committed sequence moves.
    ///
    /// The shape it replaces materialised the whole of `Base` (112,674 rows on
    /// the live vault) under a single hold of the vault-wide row-table read
    /// guard, measured at 164 ms against a 25 ms budget with every hold over
    /// budget, stalling every constellation writer behind it (#1950, #1968).
    fn live_panel_versions(&self) -> Result<BTreeSet<PanelVersionId>> {
        let mut live = BTreeSet::new();
        self.vault.walk_base_pages_atomic(
            "retired panel-version GC live set",
            crate::vault::ORPHAN_SLOT_GC_PAGE_ROWS,
            |_key, bytes| {
                live.insert(decode_constellation_base(bytes)?.panel_version);
                Ok(())
            },
        )?;
        Ok(live)
    }

    fn move_panel_version_to_cold(&self, id: PanelVersionId) -> Result<u64> {
        let Some(path) = find_panel_file(&self.hot_panel_dir, id)? else {
            return Ok(0);
        };
        crate::fsync::create_dir_all(&self.cold_panel_dir, "cold panel directory")?;
        let bytes = path.metadata().map(|meta| meta.len()).unwrap_or(0);
        let target = self.cold_panel_dir.join(
            path.file_name()
                .ok_or_else(|| calyx_core::CalyxError::disk_pressure("panel path has no name"))?,
        );
        crate::fsync::publish_path(
            &path,
            &target,
            "cold panel version",
            crate::fsync::PublishMode::CreateNew,
        )?;
        crate::fsync::sync_parent(&target, "cold panel version")?;
        Ok(bytes)
    }

    fn purge_cold_panel_version(&self, id: PanelVersionId) -> Result<u64> {
        let Some(path) = find_panel_file(&self.cold_panel_dir, id)? else {
            return Ok(0);
        };
        let bytes = path.metadata().map(|meta| meta.len()).unwrap_or(0);
        fs::remove_file(path)
            .map_err(|error| calyx_core::CalyxError::disk_pressure(error.to_string()))?;
        Ok(bytes)
    }
}

fn latest_versions_to_keep(
    records: &[PanelVersionRecord],
    keep: usize,
) -> BTreeSet<PanelVersionId> {
    let mut ids = records.iter().map(|record| record.id).collect::<Vec<_>>();
    ids.sort_unstable();
    ids.dedup();
    ids.into_iter().rev().take(keep).collect()
}

fn collect_panel_records(
    dir: &Path,
    tier: VersionTier,
    manifest_panel_path: Option<&str>,
    protect_all_without_manifest: bool,
    out: &mut Vec<PanelVersionRecord>,
) -> Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(dir)
        .map_err(|error| calyx_core::CalyxError::disk_pressure(error.to_string()))?
    {
        let entry =
            entry.map_err(|error| calyx_core::CalyxError::disk_pressure(error.to_string()))?;
        let path = entry.path();
        let Some(id) = panel_id_from_path(&path) else {
            continue;
        };
        let relative = path
            .file_name()
            .and_then(|name| name.to_str())
            .map(|name| format!("panel/{name}"));
        out.push(PanelVersionRecord {
            id,
            tier,
            ledger_referenced: protect_all_without_manifest
                || relative.as_deref() == manifest_panel_path,
            bytes: entry.metadata().map(|meta| meta.len()).unwrap_or(0),
        });
    }
    Ok(())
}

fn find_panel_file(dir: &Path, id: PanelVersionId) -> Result<Option<PathBuf>> {
    if !dir.exists() {
        return Ok(None);
    }
    for entry in fs::read_dir(dir)
        .map_err(|error| calyx_core::CalyxError::disk_pressure(error.to_string()))?
    {
        let path = entry
            .map_err(|error| calyx_core::CalyxError::disk_pressure(error.to_string()))?
            .path();
        if panel_id_from_path(&path) == Some(id) {
            return Ok(Some(path));
        }
    }
    Ok(None)
}

fn panel_id_from_path(path: &Path) -> Option<PanelVersionId> {
    let name = path.file_name()?.to_str()?;
    let rest = name.strip_prefix("panel-v")?;
    rest.get(0..8)?.parse().ok()
}

fn escape_label(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}
