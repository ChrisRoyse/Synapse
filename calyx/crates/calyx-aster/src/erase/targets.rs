use super::{EraseScope, METADATA_SUBJECT_ID, subject_metadata_value};
use crate::cf::{
    ColumnFamily, KeyRange, anchor_prefix_range, base_key, recurrence_prefix_range, slot_key,
    temporal_xterm_prefix_range, xterm_prefix_range,
};
use crate::plain_graph::{PlainGraph, graph_storage_collections};
use crate::vault::{AsterVault, encode};
use calyx_core::{Clock, Constellation, CxId, Result};
use calyx_ledger::SubjectId;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct EraseTarget {
    pub(super) cf: ColumnFamily,
    pub(super) key: Vec<u8>,
}

#[derive(Debug, Default)]
pub(super) struct EraseTargets {
    pub(super) rows: Vec<EraseTarget>,
    pub(super) records_deleted: usize,
}

pub(super) fn collect_targets<C>(
    vault: &AsterVault<C>,
    scope: &EraseScope,
    snapshot: u64,
) -> Result<EraseTargets>
where
    C: Clock,
{
    match scope {
        EraseScope::Vault => collect_vault_targets(vault, snapshot),
        EraseScope::Cx(cx_id) => collect_cx_targets(vault, snapshot, *cx_id, None),
        EraseScope::Subject(subject) => collect_subject_targets(vault, snapshot, subject),
    }
}

fn collect_vault_targets<C>(vault: &AsterVault<C>, snapshot: u64) -> Result<EraseTargets>
where
    C: Clock,
{
    let mut targets = EraseTargets::default();
    // Every static family, including the ones `supports_paged_scan` refuses.
    //
    // #1977 asked whether this loop could be migrated at all, because
    // `Scalars`, `TimeIndex` and `RawCommitment` are deliberately *not*
    // declared pageable — retaining their lookup indexes at open costs 31 s of
    // vault-open time to buy paging nothing uses (#1976). It can, and without
    // reopening that trade: `scan_cf_pages_at` pins a snapshot and pages
    // through `scan_cf_range_page_at` / `read_batch`, neither of which needs an
    // SST lookup index. The `supports_paged_scan` declaration gates
    // `scan_cf_range_page_latest` — the *latest*-view candidate pager — not
    // this one. So the loop is bounded per family with no per-family split and
    // no change to the open-time policy.
    for cf in ColumnFamily::STATIC {
        if cf == ColumnFamily::Ledger {
            continue;
        }
        vault.scan_cf_pages_at(
            snapshot,
            cf,
            crate::vault::ORPHAN_SLOT_GC_PAGE_ROWS,
            |page| -> Result<()> {
                for (key, _) in page {
                    push_unique(&mut targets.rows, cf, key);
                }
                Ok(())
            },
        )?;
    }
    // Pinned paging, not the fail-closed atomic walk: a privacy erasure must
    // complete. Refusing on any concurrent commit would leave the subject's
    // data in place on a busy daemon, which is the worse failure (#1977 ask 2).
    let mut slot_work = Vec::new();
    vault.scan_cf_pages_at(
        snapshot,
        ColumnFamily::Base,
        crate::vault::ORPHAN_SLOT_GC_PAGE_ROWS,
        |page| -> Result<()> {
            for (_, base) in page {
                slot_work.push(encode::decode_constellation_base(&base)?);
            }
            Ok(())
        },
    )?;
    for cx in slot_work {
        targets.records_deleted += 1;
        collect_slot_targets(vault, snapshot, &cx, &mut targets.rows)?;
    }
    Ok(targets)
}

fn collect_subject_targets<C>(
    vault: &AsterVault<C>,
    snapshot: u64,
    subject: &SubjectId,
) -> Result<EraseTargets>
where
    C: Clock,
{
    let expected = subject_metadata_value(subject);
    let mut targets = EraseTargets::default();
    // Selected under bounded holds, then resolved outside them: the per-record
    // `collect_cx_targets` below issues its own reads, and nesting those inside
    // the page callback would keep the outer walk's guard cadence coupled to
    // them. Pinned paging for the same reason as `collect_vault_targets` — a
    // privacy erasure must complete rather than fail closed under write load
    // (#1977 ask 2).
    let mut matched = Vec::new();
    vault.scan_cf_pages_at(
        snapshot,
        ColumnFamily::Base,
        crate::vault::ORPHAN_SLOT_GC_PAGE_ROWS,
        |page| -> Result<()> {
            for (_, base) in page {
                let cx = encode::decode_constellation_base(&base)?;
                if cx.metadata_value(METADATA_SUBJECT_ID) != Some(expected.as_str()) {
                    continue;
                }
                matched.push(cx);
            }
            Ok(())
        },
    )?;
    for cx in matched {
        let cx_targets = collect_cx_targets(vault, snapshot, cx.cx_id, Some(cx))?;
        targets.records_deleted += cx_targets.records_deleted;
        for target in cx_targets.rows {
            push_unique(&mut targets.rows, target.cf, target.key);
        }
    }
    Ok(targets)
}

fn collect_cx_targets<C>(
    vault: &AsterVault<C>,
    snapshot: u64,
    cx_id: CxId,
    base: Option<Constellation>,
) -> Result<EraseTargets>
where
    C: Clock,
{
    let mut targets = EraseTargets::default();
    let base = match base {
        Some(cx) => Some(cx),
        None => vault
            .read_cf_at(snapshot, ColumnFamily::Base, &base_key(cx_id))?
            .map(|bytes| encode::decode_constellation_base(&bytes))
            .transpose()?,
    };
    if let Some(cx) = &base {
        push_unique(&mut targets.rows, ColumnFamily::Base, base_key(cx.cx_id));
        targets.records_deleted = 1;
        collect_slot_targets(vault, snapshot, cx, &mut targets.rows)?;
    }
    collect_range_targets(
        vault,
        snapshot,
        ColumnFamily::Anchors,
        &anchor_prefix_range(cx_id),
        &mut targets.rows,
    )?;
    collect_graph_targets(vault, snapshot, cx_id, &mut targets.rows)?;
    collect_range_targets(
        vault,
        snapshot,
        ColumnFamily::XTerm,
        &xterm_prefix_range(cx_id),
        &mut targets.rows,
    )?;
    collect_range_targets(
        vault,
        snapshot,
        ColumnFamily::Recurrence,
        &recurrence_prefix_range(cx_id),
        &mut targets.rows,
    )?;
    collect_temporal_xterm_targets(vault, snapshot, cx_id, &mut targets.rows)?;
    collect_scalar_targets(vault, snapshot, cx_id, &mut targets.rows)?;
    Ok(targets)
}

fn collect_graph_targets<C>(
    vault: &AsterVault<C>,
    snapshot: u64,
    cx_id: CxId,
    targets: &mut Vec<EraseTarget>,
) -> Result<()>
where
    C: Clock,
{
    for collection in graph_storage_collections(vault, snapshot)? {
        let graph = PlainGraph::new(vault, &collection)?;
        for key in graph.erasure_keys_for_node(snapshot, cx_id)? {
            push_unique(targets, ColumnFamily::Graph, key);
        }
    }
    Ok(())
}

fn collect_slot_targets<C>(
    vault: &AsterVault<C>,
    snapshot: u64,
    cx: &Constellation,
    targets: &mut Vec<EraseTarget>,
) -> Result<()>
where
    C: Clock,
{
    for slot in cx.slots.keys().copied() {
        let key = slot_key(cx.cx_id);
        push_if_visible(
            vault,
            snapshot,
            ColumnFamily::slot(slot),
            key.clone(),
            targets,
        )?;
        push_if_visible(vault, snapshot, ColumnFamily::slot_raw(slot), key, targets)?;
    }
    Ok(())
}

fn collect_range_targets<C>(
    vault: &AsterVault<C>,
    snapshot: u64,
    cf: ColumnFamily,
    range: &KeyRange,
    targets: &mut Vec<EraseTarget>,
) -> Result<()>
where
    C: Clock,
{
    for (key, _) in vault.scan_cf_range_at(snapshot, cf, range)? {
        push_unique(targets, cf, key);
    }
    Ok(())
}

fn collect_temporal_xterm_targets<C>(
    vault: &AsterVault<C>,
    snapshot: u64,
    cx_id: CxId,
    targets: &mut Vec<EraseTarget>,
) -> Result<()>
where
    C: Clock,
{
    collect_range_targets(
        vault,
        snapshot,
        ColumnFamily::TemporalXTerm,
        &temporal_xterm_prefix_range(cx_id),
        targets,
    )?;
    let id_bytes = cx_id.as_bytes();
    for (key, _) in vault.scan_cf_at(snapshot, ColumnFamily::TemporalXTerm)? {
        if key.len() >= 32 && &key[16..32] == id_bytes {
            push_unique(targets, ColumnFamily::TemporalXTerm, key);
        }
    }
    Ok(())
}

fn collect_scalar_targets<C>(
    vault: &AsterVault<C>,
    snapshot: u64,
    cx_id: CxId,
    targets: &mut Vec<EraseTarget>,
) -> Result<()>
where
    C: Clock,
{
    for (key, _) in vault.scan_cf_at(snapshot, ColumnFamily::Scalars)? {
        if key.len() >= 20 && &key[4..20] == cx_id.as_bytes() {
            push_unique(targets, ColumnFamily::Scalars, key);
        }
    }
    Ok(())
}

fn push_if_visible<C>(
    vault: &AsterVault<C>,
    snapshot: u64,
    cf: ColumnFamily,
    key: Vec<u8>,
    targets: &mut Vec<EraseTarget>,
) -> Result<()>
where
    C: Clock,
{
    if vault.read_cf_at(snapshot, cf, &key)?.is_some() {
        push_unique(targets, cf, key);
    }
    Ok(())
}

pub(super) fn affected_cfs(targets: &[EraseTarget]) -> Vec<ColumnFamily> {
    let mut cfs = Vec::new();
    for target in targets {
        if !cfs.contains(&target.cf) {
            cfs.push(target.cf);
        }
    }
    cfs
}

fn push_unique(targets: &mut Vec<EraseTarget>, cf: ColumnFamily, key: Vec<u8>) {
    if !targets
        .iter()
        .any(|target| target.cf == cf && target.key == key)
    {
        targets.push(EraseTarget { cf, key });
    }
}
