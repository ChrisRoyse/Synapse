use super::{CompactionCatalog, SstShard, TieringPolicy};
use crate::storage_names::{
    SstName, classify_sst, ensure_unambiguous_sst_order, parse_cf_dir_name, sst_order_key,
};
use calyx_core::{CalyxError, Result};
use std::fs;
use std::path::{Path, PathBuf};

pub fn catalog_from_vault_dir(vault_dir: impl AsRef<Path>) -> Result<CompactionCatalog> {
    catalog_from_vault_tiers(vault_dir, None)
}

pub fn catalog_from_vault_tiers(
    vault_dir: impl AsRef<Path>,
    tiering_policy: Option<&TieringPolicy>,
) -> Result<CompactionCatalog> {
    catalog_from_vault_tiers_inner(vault_dir.as_ref(), tiering_policy, None)
}

/// Builds a catalog whose commit-domain files are covered by `durable_seq`.
///
/// Legacy router files have no commit-domain watermark and remain eligible
/// under the existing unambiguous-order gate. This bounded view lets live
/// compaction release the durable commit lock before the expensive directory
/// scan without adopting a concurrently committed SST that the captured
/// manifest did not yet cover.
pub(crate) fn catalog_from_vault_tiers_through_seq(
    vault_dir: impl AsRef<Path>,
    tiering_policy: Option<&TieringPolicy>,
    durable_seq: u64,
) -> Result<CompactionCatalog> {
    catalog_from_vault_tiers_inner(vault_dir.as_ref(), tiering_policy, Some(durable_seq))
}

fn catalog_from_vault_tiers_inner(
    vault_dir: &Path,
    tiering_policy: Option<&TieringPolicy>,
    durable_seq: Option<u64>,
) -> Result<CompactionCatalog> {
    let mut shards = Vec::new();
    for cf_root in tiered_cf_roots(vault_dir, tiering_policy) {
        if !cf_root.exists() {
            continue;
        }
        for entry in fs::read_dir(&cf_root).map_err(|error| {
            CalyxError::disk_pressure(format!("read compaction CF root: {error}"))
        })? {
            let path = entry
                .map_err(|error| {
                    CalyxError::disk_pressure(format!("read compaction CF entry: {error}"))
                })?
                .path();
            if !path.is_dir() {
                continue;
            }
            let name = path
                .file_name()
                .map(|value| value.to_string_lossy().to_string())
                .ok_or_else(|| {
                    CalyxError::aster_corrupt_shard(format!(
                        "compaction CF directory entry {} has no name",
                        path.display()
                    ))
                })?;
            let cf = parse_cf_dir_name(&name)?;
            for sst in list_ssts(&path)? {
                if durable_seq.is_some_and(|durable_seq| {
                    matches!(
                        classify_sst(&sst),
                        Ok(Some(
                            SstName::Flush {
                                watermark,
                                ordinal: _
                            } | SstName::DurableBatch {
                                seq: watermark,
                                index: _
                            } | SstName::Compacted { seq: watermark }
                        )) if watermark > durable_seq
                    )
                }) {
                    continue;
                }
                shards.push(SstShard::new(cf, sst, 0)?);
            }
        }
    }
    shards.sort_by(|left, right| {
        left.cf
            .name()
            .cmp(&right.cf.name())
            .then_with(|| order_key(&left.path).cmp(&order_key(&right.path)))
            .then_with(|| left.path.cmp(&right.path))
    });
    shards.dedup_by(|left, right| left.path == right.path);
    // One CF's files can span multiple tier roots; the per-directory gate in
    // list_ssts cannot see that union, so gate each CF's full shard set here.
    for chunk in shards.chunk_by(|left, right| left.cf == right.cf) {
        ensure_unambiguous_sst_order(chunk.iter().map(|shard| shard.path.as_path()))?;
    }
    Ok(CompactionCatalog::new(shards))
}

/// Lists SST files for compaction, failing closed on any `*.sst` name that
/// matches no canonical writer shape instead of compacting unknown bytes,
/// and on seq-domain-ambiguous layouts (issue #1138) — compaction merges
/// newest-wins in this order, so an ambiguous order would bake stale rows
/// into the output and reclaim the inputs holding the newer versions.
fn list_ssts(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for entry in fs::read_dir(dir)
        .map_err(|error| CalyxError::disk_pressure(format!("read compaction CF dir: {error}")))?
    {
        let path = entry
            .map_err(|error| {
                CalyxError::disk_pressure(format!("read compaction SST entry: {error}"))
            })?
            .path();
        if let Some(order) = sst_order_key(&path)? {
            files.push((order, path));
        }
    }
    ensure_unambiguous_sst_order(files.iter().map(|(_, path)| path.as_path()))?;
    files.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));
    Ok(files.into_iter().map(|(_, path)| path).collect())
}

fn order_key(path: &Path) -> crate::storage_names::SstOrderKey {
    sst_order_key(path)
        .expect("catalog contains canonical SST")
        .expect("catalog path has SST extension")
}

fn tiered_cf_roots(root: &Path, tiering_policy: Option<&TieringPolicy>) -> Vec<PathBuf> {
    let mut roots = vec![root.join("cf")];
    if let Some(policy) = tiering_policy {
        for tier_root in policy.tier_roots() {
            let cf_root = tier_root.join("cf");
            if !roots.contains(&cf_root) {
                roots.push(cf_root);
            }
        }
    }
    roots
}
