//! Offline column-family readback utility.
//!
//! Its output is supporting storage evidence only; manual FSV remains separate.
//!
//! Opens an existing Synapse storage backend strictly for read-only logical
//! scans and prints metadata-only row samples. Raw key and value material is
//! never emitted; hashes and byte lengths are enough to correlate against a
//! known synthetic input without turning the dump into a data-exfiltration
//! surface.
//!
//! Usage:
//! `cargo run -p synapse-storage --example dump_cf -- [--include-expired] <db_path> <cf_name>`
//! `cargo run -p synapse-storage --example dump_cf -- --native-cx [--reveal-metadata] <db_path> <cx_id>`
//! `cargo run -p synapse-storage --example dump_cf -- --native-source [--reveal-metadata] <db_path> <source_cf> <source_key_hex>`
//! `cargo run -p synapse-storage --example dump_cf -- --repair-bad-episode-slots <db_path>`
//! `cargo run -p synapse-storage --example dump_cf -- --check-base-roundtrip <db_path>`
//! `cargo run -p synapse-storage --example dump_cf -- --slot-kind-census <db_path> <panel_version>`
//! `cargo run -p synapse-storage --example dump_cf -- --audit-slot-hashes [--repair] <db_path>`
//! `cargo run -p synapse-storage --example dump_cf -- --migrate-pre-1776-slots [--resume] [--skip-unreproducible] <db_path>`

use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fmt,
    io::{self, Write},
    path::{Path, PathBuf},
    str::FromStr as _,
};

use calyx_aster::{
    cf::{ColumnFamily, anchor_prefix_range, base_key, scalar_id_for_key, slot_key},
    mvcc::tombstone_value,
    vault::base_rewrite::BaseRowRewrite,
    vault::encode::{
        HEADER_LEN, decode_constellation_base, decode_constellation_base_with_slot_hashes,
        decode_slot_vector, encode_constellation_base, encode_slot_vector, hash_slot_bytes,
        inspect_slot_vector,
    },
};
use calyx_core::{Constellation, CxId, SlotId, SlotVector};
use synapse_calyx::{
    SynapseCalyxCfWrite, SynapseCalyxConfig, SynapseCalyxReadOnlyVault, SynapseCalyxVault,
};
use synapse_storage::{
    StorageBackendKind,
    cf::CF_EPISODES,
    constellations::{
        META_PANEL_NAME, META_SOURCE_CF, META_SOURCE_KEY_HEX, SYN_ACTION_PANEL_NAME,
        SYN_EPISODE_PANEL_NAME, SYN_EPISODE_PANEL_VERSION, SYN_MCP_USAGE_PANEL_NAME,
        SYN_OBSERVATION_PANEL_NAME, SYN_OUTCOME_PANEL_NAME, SYN_PRE_1776_PANEL_VERSIONS,
        SYN_PROCESS_PANEL_NAME, SYN_RECURRENCE_SUBJECT_PANEL_NAME, SYN_REFLEX_PANEL_NAME,
        hex_encode, sha256_hex,
    },
    dump_cf_read_only_with_expired, scan_cf_read_only_with_expired,
};

const USAGE: &str = "usage: dump_cf --ensemble-card <db_path> <panel_version> <anchor_kind> <max_records> <min_gate_lenses> | dump_cf [--include-expired] <db_path> <cf_name> | dump_cf --native-cx [--reveal-metadata] <db_path> <cx_id> | dump_cf --native-source [--reveal-metadata] <db_path> <source_cf> <source_key_hex> | dump_cf --repair-bad-episode-slots <db_path> | dump_cf --panel-slot-audit <db_path> | dump_cf --migrate-pre-1776-slots [--resume] [--skip-unreproducible] <db_path> | dump_cf --check-base-roundtrip <db_path> | dump_cf --audit-slot-hashes [--repair] <db_path> | dump_cf --audit-source-coverage <db_path> | dump_cf --slot-kind-census <db_path> <panel_version>";
/// The episode panel's exclusive global slot block (#1776). The
/// `--repair-bad-episode-slots` mode exists for episode constellations that
/// historically wrote slots outside it; under the global allocation that is
/// exactly "outside this range", not "above some shared ceiling".
const EPISODE_SLOT_BLOCK_FIRST: u16 = 8;
const EPISODE_SLOT_BLOCK_LAST: u16 = 22;

/// Width of the identity hash `encode_constellation_base` writes straight after
/// the header, and of every per-slot hash in the slot map. Both are `blake3`
/// digests that `decode_constellation_base` skips, so they are exactly the
/// bytes a decode-and-compare cannot see.
const IDENTITY_HASH_LEN: usize = 32;
/// One `(slot_id: u16, slot_hash: [u8; 32])` slot-map entry.
const SLOT_MAP_ENTRY_LEN: usize = 2 + IDENTITY_HASH_LEN;

const fn slot_in_episode_block(slot: u16) -> bool {
    slot >= EPISODE_SLOT_BLOCK_FIRST && slot <= EPISODE_SLOT_BLOCK_LAST
}

#[allow(
    clippy::too_many_lines,
    reason = "example CLI dispatch keeps each mode visible in one small executable entry point"
)]
fn main() -> Result<(), Box<dyn Error>> {
    // Off by default so the row dumps stay machine-readable; set RUST_LOG to
    // read the engine's own structured log as evidence (e.g. the commit
    // row-family attribution, #1936).
    if std::env::var_os("RUST_LOG").is_some() {
        tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .with_writer(std::io::stderr)
            .init();
    }
    let mut args = std::env::args().skip(1).collect::<Vec<_>>();
    if args.first().is_some_and(|arg| arg == "--native-cx") {
        args.remove(0);
        let reveal_metadata = if args.first().is_some_and(|arg| arg == "--reveal-metadata") {
            args.remove(0);
            true
        } else {
            false
        };
        let mut args = args.into_iter();
        let db_path = args.next().ok_or(USAGE)?;
        let cx_id = args.next().ok_or(USAGE)?;
        if let Some(extra) = args.next() {
            return Err(format!("{USAGE}; unexpected extra argument {extra:?}").into());
        }
        return dump_native_cx(PathBuf::from(db_path), &cx_id, reveal_metadata);
    }
    if args.first().is_some_and(|arg| arg == "--native-source") {
        args.remove(0);
        let reveal_metadata = if args.first().is_some_and(|arg| arg == "--reveal-metadata") {
            args.remove(0);
            true
        } else {
            false
        };
        let mut args = args.into_iter();
        let db_path = args.next().ok_or(USAGE)?;
        let source_cf = args.next().ok_or(USAGE)?;
        let source_key_hex = args.next().ok_or(USAGE)?;
        if let Some(extra) = args.next() {
            return Err(format!("{USAGE}; unexpected extra argument {extra:?}").into());
        }
        return dump_native_source(
            PathBuf::from(db_path),
            &source_cf,
            &source_key_hex,
            reveal_metadata,
        );
    }
    if args
        .first()
        .is_some_and(|arg| arg == "--repair-bad-episode-slots")
    {
        args.remove(0);
        let mut args = args.into_iter();
        let db_path = args.next().ok_or(USAGE)?;
        if let Some(extra) = args.next() {
            return Err(format!("{USAGE}; unexpected extra argument {extra:?}").into());
        }
        return repair_bad_episode_slots(PathBuf::from(db_path));
    }
    if args
        .first()
        .is_some_and(|arg| arg == "--migrate-pre-1776-slots")
    {
        args.remove(0);
        let mut options = MigrationOptions {
            resume: false,
            skip_unreproducible: false,
        };
        while let Some(flag) = args.first() {
            match flag.as_str() {
                "--resume" => options.resume = true,
                "--skip-unreproducible" => options.skip_unreproducible = true,
                _ => break,
            }
            args.remove(0);
        }
        let mut args = args.into_iter();
        let db_path = args.next().ok_or(USAGE)?;
        if let Some(extra) = args.next() {
            return Err(format!("{USAGE}; unexpected extra argument {extra:?}").into());
        }
        return migrate_pre_1776_slots(PathBuf::from(db_path), options);
    }
    if args
        .first()
        .is_some_and(|arg| arg == "--check-base-roundtrip")
    {
        args.remove(0);
        let mut args = args.into_iter();
        let db_path = args.next().ok_or(USAGE)?;
        if let Some(extra) = args.next() {
            return Err(format!("{USAGE}; unexpected extra argument {extra:?}").into());
        }
        return check_base_roundtrip(PathBuf::from(db_path));
    }
    if args
        .first()
        .is_some_and(|arg| arg == "--audit-source-coverage")
    {
        args.remove(0);
        let mut args = args.into_iter();
        let db_path = args.next().ok_or(USAGE)?;
        if let Some(extra) = args.next() {
            return Err(format!("{USAGE}; unexpected extra argument {extra:?}").into());
        }
        return audit_source_coverage(PathBuf::from(db_path));
    }
    if args.first().is_some_and(|arg| arg == "--audit-slot-hashes") {
        args.remove(0);
        let repair = if args.first().is_some_and(|arg| arg == "--repair") {
            args.remove(0);
            true
        } else {
            false
        };
        let mut args = args.into_iter();
        let db_path = args.next().ok_or(USAGE)?;
        if let Some(extra) = args.next() {
            return Err(format!("{USAGE}; unexpected extra argument {extra:?}").into());
        }
        return audit_slot_hashes(PathBuf::from(db_path), repair);
    }
    if args.first().is_some_and(|arg| arg == "--panel-slot-audit") {
        args.remove(0);
        let mut args = args.into_iter();
        let db_path = args.next().ok_or(USAGE)?;
        if let Some(extra) = args.next() {
            return Err(format!("{USAGE}; unexpected extra argument {extra:?}").into());
        }
        return panel_slot_audit(PathBuf::from(db_path));
    }
    if args.first().is_some_and(|arg| arg == "--slot-kind-census") {
        args.remove(0);
        let mut args = args.into_iter();
        let db_path = args.next().ok_or(USAGE)?;
        let panel_version = args
            .next()
            .ok_or(USAGE)?
            .parse::<u32>()
            .map_err(|error| format!("{USAGE}; panel_version must be a u32: {error}"))?;
        if let Some(extra) = args.next() {
            return Err(format!("{USAGE}; unexpected extra argument {extra:?}").into());
        }
        return slot_kind_census(PathBuf::from(db_path), panel_version);
    }
    if args.first().is_some_and(|arg| arg == "--ensemble-card") {
        args.remove(0);
        let mut args = args.into_iter();
        let db_path = args.next().ok_or(USAGE)?;
        let panel_version = args
            .next()
            .ok_or(USAGE)?
            .parse::<u32>()
            .map_err(|error| format!("{USAGE}; panel_version must be a u32: {error}"))?;
        let anchor_kind = args.next().ok_or(USAGE)?;
        let max_records = args
            .next()
            .ok_or(USAGE)?
            .parse::<usize>()
            .map_err(|error| format!("{USAGE}; max_records must be a usize: {error}"))?;
        let min_gate_lenses = args
            .next()
            .ok_or(USAGE)?
            .parse::<usize>()
            .map_err(|error| format!("{USAGE}; min_gate_lenses must be a usize: {error}"))?;
        if let Some(extra) = args.next() {
            return Err(format!("{USAGE}; unexpected extra argument {extra:?}").into());
        }
        return ensemble_card_report(
            PathBuf::from(db_path),
            panel_version,
            &anchor_kind,
            max_records,
            min_gate_lenses,
        );
    }
    let include_expired = if args.first().is_some_and(|arg| arg == "--include-expired") {
        args.remove(0);
        true
    } else {
        false
    };
    let mut args = args.into_iter();
    let db_path = args.next().ok_or(USAGE)?;
    let cf_name = args.next().ok_or(USAGE)?;
    if let Some(extra) = args.next() {
        return Err(format!("{USAGE}; unexpected extra argument {extra:?}").into());
    }

    let dump = dump_cf_read_only_with_expired(
        Path::new(&db_path),
        synapse_core::SCHEMA_VERSION,
        StorageBackendKind::Calyx,
        &cf_name,
        include_expired,
    )?;
    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    if !write_stdout_line(
        &mut stdout,
        format_args!(
            "dump_cf db_path={db_path} cf={} backend={} mode=read_only include_expired={} row_count={}",
            dump.cf_name,
            dump.backend.as_str(),
            include_expired,
            dump.row_count
        ),
    )? {
        return Ok(());
    }
    for (index, row) in dump.rows.iter().enumerate() {
        if !write_stdout_line(
            &mut stdout,
            format_args!(
                "row[{index}] key_len_bytes={} key_sha256={} key_material_omitted={} value_len_bytes={} value_sha256={} value_encoding={} value_content_omitted={} redaction_policy={}",
                row.key_len_bytes,
                row.key_sha256,
                row.key_material_omitted,
                row.value_len_bytes,
                row.value_sha256,
                row.value_encoding,
                row.value_content_omitted,
                row.redaction_policy
            ),
        )? {
            return Ok(());
        }
    }
    Ok(())
}

#[allow(
    clippy::too_many_lines,
    clippy::needless_pass_by_value,
    reason = "repair mode owns its path for repeated vault opens and prints the full before/write/readback flow inline"
)]
fn repair_bad_episode_slots(db_path: PathBuf) -> Result<(), Box<dyn Error>> {
    let vault = SynapseCalyxVault::open(SynapseCalyxConfig::from_vault_dir(db_path.clone()))?;
    let before_seq = vault.latest_seq();
    let before_base_rows = vault.scan_cf_at(before_seq, ColumnFamily::Base)?;
    let mut candidates = Vec::new();

    for (key, value) in before_base_rows {
        let constellation = decode_constellation_base(&value).map_err(|error| {
            format!(
                "REPAIR_BASE_DECODE_FAILED key_hex={} value_len_bytes={} value_sha256={}: {error}",
                hex_encode(&key),
                value.len(),
                sha256_hex(&value)
            )
        })?;
        if !is_bad_episode_slot_constellation(&constellation) {
            continue;
        }
        candidates.push(RepairCandidate {
            cx_id: constellation.cx_id,
            source_key_hex: constellation
                .metadata
                .get(META_SOURCE_KEY_HEX)
                .cloned()
                .unwrap_or_default(),
            slot_ids: constellation.slots.keys().copied().collect(),
        });
    }

    let mut targets = Vec::new();
    let mut duplicate_targets = 0_u64;
    for candidate in &candidates {
        let candidate_targets =
            collect_repair_targets(&vault, before_seq, candidate.cx_id, &candidate.slot_ids)?;
        for target in candidate_targets {
            if targets.iter().any(|existing: &RepairTarget| {
                existing.cf == target.cf && existing.key == target.key
            }) {
                duplicate_targets = duplicate_targets.saturating_add(1);
                continue;
            }
            targets.push(target);
        }
    }

    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    write_stdout_line(
        &mut stdout,
        format_args!(
            "repair_bad_episode_slots db_path={} mode=writeable before_seq={} candidate_count={} target_count={} duplicate_target_count={}",
            db_path.display(),
            before_seq,
            candidates.len(),
            targets.len(),
            duplicate_targets,
        ),
    )?;
    for (index, candidate) in candidates.iter().enumerate() {
        let slot_ids = candidate
            .slot_ids
            .iter()
            .map(|slot| slot.get())
            .map(|slot| slot.to_string())
            .collect::<Vec<_>>()
            .join(",");
        write_stdout_line(
            &mut stdout,
            format_args!(
                "repair_candidate index={} cx_id={} source_cf={} source_key_hex={} slot_ids={}",
                index, candidate.cx_id, CF_EPISODES, candidate.source_key_hex, slot_ids,
            ),
        )?;
    }

    let tombstone = tombstone_value();
    let writes = targets
        .iter()
        .map(|target| SynapseCalyxCfWrite::new(target.cf, target.key.clone(), tombstone.clone()))
        .collect::<Vec<_>>();
    let commit_seq = vault.write_cf_batch(writes)?;
    vault.flush()?;
    let after_seq = vault.latest_seq();
    let close_readback = vault.close("repair_bad_episode_slots")?;
    drop(stdout);

    let readback = SynapseCalyxReadOnlyVault::open_existing_with_cfs(
        SynapseCalyxConfig::from_vault_dir(db_path.clone()),
        None,
    )?;
    let after_base_rows = readback.scan_cf_at(readback.latest_seq(), ColumnFamily::Base)?;
    let remaining_bad = count_bad_episode_slots(&after_base_rows)?;
    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    write_stdout_line(
        &mut stdout,
        format_args!(
            "repair_commit commit_seq={} after_seq={} close_safe_to_unlock={} close_latest_seq={:?}",
            commit_seq, after_seq, close_readback.safe_to_unlock, close_readback.latest_seq,
        ),
    )?;
    write_stdout_line(
        &mut stdout,
        format_args!(
            "repair_readback readonly_seq={} remaining_bad_episode_slot_constellations={}",
            readback.latest_seq(),
            remaining_bad
        ),
    )?;
    if remaining_bad != 0 {
        return Err(format!(
            "REPAIR_BAD_EPISODE_SLOTS_READBACK_FAILED remaining_bad={remaining_bad}"
        )
        .into());
    }
    Ok(())
}

fn is_bad_episode_slot_constellation(constellation: &calyx_core::Constellation) -> bool {
    constellation.panel_version == SYN_EPISODE_PANEL_VERSION
        && constellation
            .metadata
            .get(META_PANEL_NAME)
            .map(String::as_str)
            == Some(SYN_EPISODE_PANEL_NAME)
        && constellation
            .metadata
            .get(META_SOURCE_CF)
            .map(String::as_str)
            == Some(CF_EPISODES)
        && constellation
            .slots
            .keys()
            .any(|slot| !slot_in_episode_block(slot.get()))
}

fn count_bad_episode_slots(base_rows: &[(Vec<u8>, Vec<u8>)]) -> Result<usize, Box<dyn Error>> {
    let mut count = 0_usize;
    for (key, value) in base_rows {
        let constellation = decode_constellation_base(value).map_err(|error| {
            format!(
                "REPAIR_READBACK_BASE_DECODE_FAILED key_hex={} value_len_bytes={} value_sha256={}: {error}",
                hex_encode(key),
                value.len(),
                sha256_hex(value)
            )
        })?;
        if is_bad_episode_slot_constellation(&constellation) {
            count += 1;
        }
    }
    Ok(count)
}

fn collect_repair_targets(
    vault: &SynapseCalyxVault,
    snapshot: u64,
    cx_id: CxId,
    slot_ids: &[SlotId],
) -> Result<Vec<RepairTarget>, Box<dyn Error>> {
    let mut targets = Vec::new();
    push_if_visible(
        &mut targets,
        vault,
        snapshot,
        ColumnFamily::Base,
        base_key(cx_id),
    )?;
    for &slot_id in slot_ids {
        if !slot_in_episode_block(slot_id.get()) {
            continue;
        }
        let key = slot_key(cx_id);
        push_if_visible(
            &mut targets,
            vault,
            snapshot,
            ColumnFamily::slot(slot_id),
            key.clone(),
        )?;
        push_if_visible(
            &mut targets,
            vault,
            snapshot,
            ColumnFamily::slot_raw(slot_id),
            key,
        )?;
    }
    for (key, _) in vault.scan_cf_at(snapshot, ColumnFamily::Scalars)? {
        if key.len() >= 20 && &key[4..20] == cx_id.as_bytes() {
            push_unique(&mut targets, ColumnFamily::Scalars, key);
        }
    }
    for (key, _) in
        vault.scan_cf_range_at(snapshot, ColumnFamily::Anchors, &anchor_prefix_range(cx_id))?
    {
        push_unique(&mut targets, ColumnFamily::Anchors, key);
    }
    Ok(targets)
}

fn push_if_visible(
    targets: &mut Vec<RepairTarget>,
    vault: &SynapseCalyxVault,
    snapshot: u64,
    cf: ColumnFamily,
    key: Vec<u8>,
) -> Result<(), Box<dyn Error>> {
    if vault.read_cf_at(snapshot, cf, &key)?.is_some() {
        push_unique(targets, cf, key);
    }
    Ok(())
}

fn push_unique(targets: &mut Vec<RepairTarget>, cf: ColumnFamily, key: Vec<u8>) {
    if !targets
        .iter()
        .any(|target| target.cf == cf && target.key == key)
    {
        targets.push(RepairTarget { cf, key });
    }
}

#[derive(Debug)]
struct RepairCandidate {
    cx_id: CxId,
    source_key_hex: String,
    slot_ids: Vec<SlotId>,
}

#[derive(Debug)]
struct RepairTarget {
    cf: ColumnFamily,
    key: Vec<u8>,
}

#[allow(
    clippy::needless_pass_by_value,
    reason = "source dump owns the path while chaining into per-constellation readbacks"
)]
fn dump_native_source(
    db_path: PathBuf,
    source_cf: &str,
    source_key_hex: &str,
    reveal_metadata: bool,
) -> Result<(), Box<dyn Error>> {
    let source_key_hex = canonical_hex(source_key_hex)?;
    let vault = SynapseCalyxReadOnlyVault::open_existing_with_cfs(
        SynapseCalyxConfig::from_vault_dir(db_path.clone()),
        None,
    )?;
    let snapshot = vault.latest_seq();
    let base_rows = vault.scan_cf_at(snapshot, ColumnFamily::Base)?;
    let mut scanned = 0_u64;
    let mut matches = Vec::new();

    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    if !write_stdout_line(
        &mut stdout,
        format_args!(
            "native_source db_path={} mode=read_only vault_id={} snapshot={} source_cf={} source_key_hex={}",
            db_path.display(),
            vault.vault_id(),
            snapshot,
            source_cf,
            source_key_hex,
        ),
    )? {
        return Ok(());
    }

    for (key, value) in base_rows {
        scanned = scanned.saturating_add(1);
        let constellation = decode_constellation_base(&value).map_err(|error| {
            format!(
                "NATIVE_BASE_DECODE_FAILED key_hex={} value_len_bytes={} value_sha256={}: {error}",
                hex_encode(&key),
                value.len(),
                sha256_hex(&value)
            )
        })?;
        let Some(base_source_cf) = constellation.metadata.get(META_SOURCE_CF) else {
            continue;
        };
        let Some(base_source_key_hex) = constellation.metadata.get(META_SOURCE_KEY_HEX) else {
            continue;
        };
        if base_source_cf != source_cf || base_source_key_hex != &source_key_hex {
            continue;
        }
        let cx_id = constellation.cx_id.to_string();
        matches.push(cx_id.clone());
        if !write_stdout_line(
            &mut stdout,
            format_args!(
                "source_match index={} cx_id={} base_key_hex={} value_len_bytes={} value_sha256={} panel_version={} slot_count={} scalar_count={}",
                matches.len() - 1,
                cx_id,
                hex_encode(&key),
                value.len(),
                sha256_hex(&value),
                constellation.panel_version,
                constellation.slots.len(),
                constellation.scalars.len(),
            ),
        )? {
            return Ok(());
        }
    }

    if !write_stdout_line(
        &mut stdout,
        format_args!(
            "native_source_result base_rows_scanned={} match_count={}",
            scanned,
            matches.len()
        ),
    )? {
        return Ok(());
    }
    drop(stdout);

    for cx_id in matches {
        dump_native_cx(db_path.clone(), &cx_id, reveal_metadata)?;
    }
    Ok(())
}

/// Read-only measurement of the #1776 physical slot collision.
///
/// Synapse now allocates each panel an exclusive block of GLOBAL slot ids, but
/// Calyx persists every vector in a global `cf/slot_<id>` column family keyed
/// only by `CxId`, so rows written before that allocation can still sit in
/// another panel's column family. This walks the Base CF, groups each
/// constellation's declared slot ids by its `synapse_panel_name`, and reports
/// every slot id claimed by more than one panel — each one a physical column
/// family holding vectors of different shapes and meanings.
///
/// Read-only: it does not take the writer lock, so it can run against the live
/// vault while the daemon owns it.
#[allow(
    clippy::needless_pass_by_value,
    reason = "audit owns the path while opening the vault read-only"
)]
fn panel_slot_audit(db_path: PathBuf) -> Result<(), Box<dyn Error>> {
    let vault = SynapseCalyxReadOnlyVault::open_existing_with_cfs(
        SynapseCalyxConfig::from_vault_dir(db_path.clone()),
        None,
    )?;
    let snapshot = vault.latest_seq();
    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    write_stdout_line(
        &mut stdout,
        format_args!(
            "panel_slot_audit db_path={} mode=read_only vault_id={} snapshot={}",
            db_path.display(),
            vault.vault_id(),
            snapshot
        ),
    )?;

    // panel -> slot id -> row count, and slot id -> panel -> row count.
    let mut per_panel: BTreeMap<String, BTreeMap<u16, u64>> = BTreeMap::new();
    let mut per_slot: BTreeMap<u16, BTreeMap<String, u64>> = BTreeMap::new();
    let mut rows = 0_u64;
    let mut undecodable = 0_u64;
    let mut unlabelled = 0_u64;

    for (_key, value) in vault.scan_cf_at(snapshot, ColumnFamily::Base)? {
        rows += 1;
        let Ok(constellation) = decode_constellation_base(&value) else {
            undecodable += 1;
            continue;
        };
        let panel = constellation.metadata.get(META_PANEL_NAME).map_or_else(
            || {
                unlabelled += 1;
                "<unlabelled>".to_owned()
            },
            Clone::clone,
        );
        for slot in constellation.slots.keys() {
            *per_panel
                .entry(panel.clone())
                .or_default()
                .entry(slot.get())
                .or_default() += 1;
            *per_slot
                .entry(slot.get())
                .or_default()
                .entry(panel.clone())
                .or_default() += 1;
        }
    }

    write_stdout_line(
        &mut stdout,
        format_args!(
            "base_rows={rows} undecodable={undecodable} unlabelled_panel={unlabelled} panels={} distinct_slot_ids={}",
            per_panel.len(),
            per_slot.len()
        ),
    )?;

    for (panel, slots) in &per_panel {
        let ids = slots
            .keys()
            .map(u16::to_string)
            .collect::<Vec<_>>()
            .join(",");
        let total = slots.values().sum::<u64>();
        write_stdout_line(
            &mut stdout,
            format_args!(
                "panel name={panel} slot_count={} slot_ids={ids} slot_rows={total}",
                slots.len()
            ),
        )?;
    }

    let mut collisions = 0_u64;
    for (slot, panels) in &per_slot {
        if panels.len() < 2 {
            continue;
        }
        collisions += 1;
        let detail = panels
            .iter()
            .map(|(panel, count)| format!("{panel}={count}"))
            .collect::<Vec<_>>()
            .join(" ");
        write_stdout_line(
            &mut stdout,
            format_args!(
                "COLLISION physical_cf=slot_{slot:02} panel_count={} {detail}",
                panels.len()
            ),
        )?;
    }
    write_stdout_line(&mut stdout, format_args!("collision_slot_ids={collisions}"))?;
    Ok(())
}

/// Per-slot vector-kind census for one panel version, hydrated from the
/// physical per-slot column families (#1939).
///
/// This is the instrument behind the #1939 finding. A `Base` row alone cannot
/// answer it — every slot it decodes to is `SlotVector::Absent` because the
/// vectors live in `cf/slot_<id>` (#1894) — so the census hydrates each
/// declared slot from its own column family and classifies the bytes actually
/// stored there.
///
/// For a sparse slot it also reports the **corpus-observed support**: the set of
/// distinct occupied indices across every scanned record. That number decides
/// whether the intelligence stack can carry the lens, because densifying a
/// sparse column over exactly its observed support is a lossless, exact
/// transformation — every dropped index is zero in every record, so it moves no
/// dot product, no norm, no Chebyshev distance and no interned identity.
///
/// Read-only: it does not take the writer lock, so it can run against the live
/// vault while the daemon owns it.
#[allow(
    clippy::too_many_lines,
    clippy::needless_pass_by_value,
    reason = "the census owns its path and prints the whole scan and per-slot tally inline"
)]
fn slot_kind_census(db_path: PathBuf, panel_version: u32) -> Result<(), Box<dyn Error>> {
    let vault = SynapseCalyxReadOnlyVault::open_existing_with_cfs(
        SynapseCalyxConfig::from_vault_dir(db_path.clone()),
        None,
    )?;
    let snapshot = vault.latest_seq();
    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    write_stdout_line(
        &mut stdout,
        format_args!(
            "slot_kind_census db_path={} mode=read_only vault_id={} snapshot={snapshot} panel_version={panel_version}",
            db_path.display(),
            vault.vault_id(),
        ),
    )?;

    #[derive(Default)]
    struct SlotCensus {
        declared_rows: u64,
        dense: u64,
        sparse: u64,
        multi: u64,
        absent: u64,
        missing_cf_row: u64,
        undecodable: u64,
        dims: BTreeSet<u32>,
        /// Distinct occupied indices across the corpus — the width a lossless
        /// densification would need.
        observed_support: BTreeSet<u32>,
        /// Distinct whole-vector identities: what a discrete estimator would
        /// see as the column's cardinality.
        distinct_values: BTreeSet<Vec<u8>>,
        nnz_max: usize,
    }

    let mut per_slot: BTreeMap<u16, SlotCensus> = BTreeMap::new();
    let mut panel_rows = 0_u64;
    let mut scanned = 0_u64;
    let mut undecodable_base = 0_u64;

    for (_key, value) in vault.scan_cf_at(snapshot, ColumnFamily::Base)? {
        scanned += 1;
        let Ok(constellation) = decode_constellation_base(&value) else {
            undecodable_base += 1;
            continue;
        };
        if constellation.panel_version != panel_version {
            continue;
        }
        panel_rows += 1;
        let key = slot_key(constellation.cx_id);
        for slot_id in constellation.slots.keys() {
            let census = per_slot.entry(slot_id.get()).or_default();
            census.declared_rows += 1;
            let Some(bytes) = vault.read_cf_at(snapshot, ColumnFamily::slot(*slot_id), &key)?
            else {
                census.missing_cf_row += 1;
                continue;
            };
            let Ok(vector) = decode_slot_vector(&bytes) else {
                census.undecodable += 1;
                continue;
            };
            match &vector {
                SlotVector::Dense { dim, data } => {
                    census.dense += 1;
                    census.dims.insert(*dim);
                    census.nnz_max = census.nnz_max.max(data.len());
                }
                SlotVector::Sparse { dim, entries } => {
                    census.sparse += 1;
                    census.dims.insert(*dim);
                    census.nnz_max = census.nnz_max.max(entries.len());
                    for entry in entries {
                        census.observed_support.insert(entry.idx);
                    }
                }
                SlotVector::Multi { token_dim, tokens } => {
                    census.multi += 1;
                    census.dims.insert(*token_dim);
                    census.nnz_max = census.nnz_max.max(tokens.len());
                }
                SlotVector::Absent { .. } => census.absent += 1,
            }
            // Canonical identity of the whole vector, so the reported
            // cardinality is exactly what an interning estimator would see.
            census
                .distinct_values
                .insert(hash_slot_bytes(&bytes).to_vec());
        }
    }

    write_stdout_line(
        &mut stdout,
        format_args!(
            "base_rows_scanned={scanned} panel_rows={panel_rows} undecodable_base={undecodable_base} declared_slots={}",
            per_slot.len()
        ),
    )?;
    for (slot, census) in &per_slot {
        let kind = if census.sparse > 0 && census.dense == 0 {
            "sparse"
        } else if census.dense > 0 && census.sparse == 0 {
            "dense"
        } else if census.multi > 0 {
            "multi"
        } else if census.dense > 0 && census.sparse > 0 {
            "MIXED"
        } else {
            "none"
        };
        let dims = census
            .dims
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(",");
        write_stdout_line(
            &mut stdout,
            format_args!(
                "slot={slot} kind={kind} declared_rows={} dense={} sparse={} multi={} absent={} missing_cf_row={} undecodable={} dims=[{dims}] nnz_max={} observed_support={} distinct_values={}",
                census.declared_rows,
                census.dense,
                census.sparse,
                census.multi,
                census.absent,
                census.missing_cf_row,
                census.undecodable,
                census.nnz_max,
                census.observed_support.len(),
                census.distinct_values.len()
            ),
        )?;
    }
    let sparse_slots = per_slot.values().filter(|census| census.sparse > 0).count();
    write_stdout_line(
        &mut stdout,
        format_args!(
            "summary declared_slots={} sparse_slots={sparse_slots} dense_slots={}",
            per_slot.len(),
            per_slot.values().filter(|census| census.dense > 0).count()
        ),
    )?;
    Ok(())
}

/// One pre-#1776 panel and the exclusive global slot block it owns today.
///
/// This restates the seven blocks the migration needs from `PANEL_SLOT_BLOCKS`
/// in `crates/synapse-storage/src/constellations.rs`, which is private and so
/// unreachable from an example. The ids are the literal ones #1776 allocated —
/// never recomputed — and [`verify_pre_1776_block_table`] fails the run closed
/// if `SYN_PRE_1776_PANEL_VERSIONS` ever names a panel this table does not, so
/// the two cannot drift apart silently.
struct Pre1776PanelBlock {
    panel: &'static str,
    first: u16,
    last: u16,
}

const PRE_1776_PANEL_BLOCKS: &[Pre1776PanelBlock] = &[
    Pre1776PanelBlock {
        panel: SYN_ACTION_PANEL_NAME,
        first: 48,
        last: 52,
    },
    Pre1776PanelBlock {
        panel: SYN_REFLEX_PANEL_NAME,
        first: 53,
        last: 59,
    },
    Pre1776PanelBlock {
        panel: SYN_PROCESS_PANEL_NAME,
        first: 60,
        last: 66,
    },
    Pre1776PanelBlock {
        panel: SYN_OBSERVATION_PANEL_NAME,
        first: 67,
        last: 74,
    },
    Pre1776PanelBlock {
        panel: SYN_OUTCOME_PANEL_NAME,
        first: 75,
        last: 81,
    },
    Pre1776PanelBlock {
        panel: SYN_MCP_USAGE_PANEL_NAME,
        first: 82,
        last: 93,
    },
    Pre1776PanelBlock {
        panel: SYN_RECURRENCE_SUBJECT_PANEL_NAME,
        first: 94,
        last: 95,
    },
];

impl Pre1776PanelBlock {
    const fn width(&self) -> u16 {
        self.last.saturating_sub(self.first).saturating_add(1)
    }

    const fn contains(&self, slot: u16) -> bool {
        slot >= self.first && slot <= self.last
    }
}

/// How one Base row relates to the #1776 migration.
enum Pre1776Row {
    /// Not written under a superseded panel version; the migration ignores it.
    NotSuperseded,
    /// Superseded but declares no slots, so it holds no colliding vector.
    NoSlots,
    /// Superseded and already sitting entirely inside its panel's block.
    AlreadyInBlock,
    /// Superseded and still sitting entirely outside its panel's block.
    Pending {
        block: &'static Pre1776PanelBlock,
        old_slot_ids: Vec<SlotId>,
    },
}

/// A constellation still to move, captured from the planning scan.
struct MigrationCandidate {
    cx_id: CxId,
    panel: &'static str,
    block: &'static Pre1776PanelBlock,
    old_slot_ids: Vec<SlotId>,
    base_value: Vec<u8>,
}

/// One slot's move, with the exact durable bytes that must survive it.
struct SlotMove {
    old: SlotId,
    new: SlotId,
    quantized: Vec<u8>,
    raw: Option<Vec<u8>>,
    vector: SlotVector,
}

/// Migrates pre-#1776 constellations out of the colliding physical slot CFs
/// (#1878).
///
/// #1776 gave every panel an exclusive block of GLOBAL slot ids but did not
/// move the rows already written under the old panel-local ids, which still sit
/// in `cf/slot_01`..`cf/slot_08` alongside timeline's and episode's vectors.
/// This walks the Base CF and, for each superseded row, copies its vectors and
/// `.raw` sidecars to its panel's block, tombstones the vacated rows, and
/// rewrites the Base row's slot map — one `write_cf_batch` per constellation,
/// so a crash leaves each constellation wholly old or wholly new.
///
/// `panel_version` is deliberately left alone. `CxId::from_input(input_bytes,
/// panel_version, vault_salt)` makes identity a function of the version, so
/// rewriting it in place would produce a row whose stored `cx_id` is not the id
/// its own derivation rule yields. `validate_panel_slot_allocation` resolves a
/// panel's block by `synapse_panel_name`, not by version, so a row that keeps
/// `panel_version=1666001` and `panel=syn-action-v1` validates correctly
/// against block 48..=52.
///
/// The resume state is derived from the data, not from a cursor file: a
/// constellation is pending exactly while its own Base row still declares
/// out-of-block slot ids. A second run therefore finds nothing to do, and an
/// interrupted run resumes with no external bookkeeping to lose.
#[allow(
    clippy::too_many_lines,
    clippy::needless_pass_by_value,
    reason = "the migration owns its path across the writeable pass and the independent read-only readback, and prints the full plan/move/readback flow inline so the run is auditable"
)]
fn migrate_pre_1776_slots(
    db_path: PathBuf,
    options: MigrationOptions,
) -> Result<(), Box<dyn Error>> {
    let MigrationOptions {
        resume,
        skip_unreproducible,
    } = options;
    verify_pre_1776_block_table()?;
    let vault = SynapseCalyxVault::open(SynapseCalyxConfig::from_vault_dir(db_path.clone()))?;
    let before_seq = vault.latest_seq();
    let before_base_rows = vault.scan_cf_at(before_seq, ColumnFamily::Base)?;
    let before_base_row_count = before_base_rows.len();

    let mut pending = Vec::new();
    let mut already_in_block = 0_u64;
    let mut slotless = 0_u64;
    for (key, value) in before_base_rows {
        let constellation = decode_base_for_migration(&key, &value)?;
        match classify_pre_1776_row(&constellation)? {
            Pre1776Row::NotSuperseded => {}
            Pre1776Row::NoSlots => slotless = slotless.saturating_add(1),
            Pre1776Row::AlreadyInBlock => already_in_block = already_in_block.saturating_add(1),
            Pre1776Row::Pending {
                block,
                old_slot_ids,
            } => pending.push(MigrationCandidate {
                cx_id: constellation.cx_id,
                panel: block.panel,
                block,
                old_slot_ids,
                base_value: value,
            }),
        }
    }

    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    write_stdout_line(
        &mut stdout,
        format_args!(
            "migrate_pre_1776_slots db_path={} mode=writeable resume={resume} before_seq={before_seq} base_rows={before_base_row_count} pending={} already_in_block={already_in_block} superseded_without_slots={slotless}",
            db_path.display(),
            pending.len(),
        ),
    )?;

    if !pending.is_empty() && already_in_block != 0 && !resume {
        return Err(format!(
            "SYNAPSE_MIGRATE_PRE_1776_PARTIAL_STATE: this vault already holds {already_in_block} \
             migrated pre-#1776 constellation(s) and {} still-unmigrated one(s), which is the \
             state an interrupted run leaves behind. Re-run with --resume to continue it \
             deliberately (issue #1878)",
            pending.len()
        )
        .into());
    }

    let mut moved_constellations = 0_u64;
    let mut moved_slot_rows = 0_u64;
    let mut moved_raw_rows = 0_u64;
    let mut skipped = 0_u64;
    for (index, candidate) in pending.iter().enumerate() {
        let report = match migrate_one_constellation(&vault, candidate) {
            Ok(report) => report,
            // The guard is never relaxed. `--skip-unreproducible` only chooses
            // between "stop the whole run" and "leave this one row exactly as it
            // is and name it", and a skipped row is still reported as
            // unmigrated at the end. Nothing is written for it either way.
            Err(error) if skip_unreproducible => {
                skipped = skipped.saturating_add(1);
                write_stdout_line(
                    &mut stdout,
                    format_args!(
                        "unmigrated index={index} cx_id={} panel={} old_slot_ids={} error={error}",
                        candidate.cx_id,
                        candidate.panel,
                        join_slot_ids(&candidate.old_slot_ids),
                    ),
                )?;
                continue;
            }
            Err(error) => return Err(error),
        };
        moved_constellations = moved_constellations.saturating_add(1);
        moved_slot_rows = moved_slot_rows.saturating_add(report.slot_rows);
        moved_raw_rows = moved_raw_rows.saturating_add(report.raw_rows);
        write_stdout_line(
            &mut stdout,
            format_args!(
                "migrated index={index} cx_id={} panel={} panel_version={} old_slot_ids={} new_slot_ids={} slot_rows={} raw_rows={} base_value_len_bytes={} base_value_sha256={} commit_seq={}",
                candidate.cx_id,
                candidate.panel,
                report.panel_version,
                join_slot_ids(&report.old_slot_ids),
                join_slot_ids(&report.new_slot_ids),
                report.slot_rows,
                report.raw_rows,
                report.base_value_len_bytes,
                report.base_value_sha256,
                report.commit_seq,
            ),
        )?;
        // Per-slot digests, so the "vector bytes are byte-identical before and
        // after" check is externally checkable for every moved row and not just
        // the one that gets spot-read: these are the same value_sha256 that
        // `--native-cx` prints for the slot.
        for slot in &report.slots {
            write_stdout_line(
                &mut stdout,
                format_args!(
                    "migrated_slot cx_id={} panel={} old_slot_id={} new_slot_id={} value_len_bytes={} value_sha256={} raw_len_bytes={} raw_sha256={}",
                    candidate.cx_id,
                    candidate.panel,
                    slot.old,
                    slot.new,
                    slot.value_len_bytes,
                    slot.value_sha256,
                    slot.raw_len_bytes
                        .map_or_else(|| "absent".to_owned(), |len| len.to_string()),
                    slot.raw_sha256.as_deref().unwrap_or("absent"),
                ),
            )?;
        }
    }

    vault.flush()?;
    let after_seq = vault.latest_seq();
    let close_readback = vault.close("migrate_pre_1776_slots")?;
    write_stdout_line(
        &mut stdout,
        format_args!(
            "migrate_commit moved_constellations={moved_constellations} moved_slot_rows={moved_slot_rows} moved_raw_rows={moved_raw_rows} unmigrated={skipped} after_seq={after_seq} close_safe_to_unlock={} close_latest_seq={:?}",
            close_readback.safe_to_unlock, close_readback.latest_seq,
        ),
    )?;
    drop(stdout);

    // Independent readback through a fresh read-only handle: the migration is
    // only done when a process that never held the writer lock agrees.
    let readback = SynapseCalyxReadOnlyVault::open_existing_with_cfs(
        SynapseCalyxConfig::from_vault_dir(db_path),
        None,
    )?;
    let readback_seq = readback.latest_seq();
    let after_base_rows = readback.scan_cf_at(readback_seq, ColumnFamily::Base)?;
    let after_base_row_count = after_base_rows.len();
    let mut remaining_pending = 0_u64;
    for (key, value) in after_base_rows {
        let constellation = decode_base_for_migration(&key, &value)?;
        if matches!(
            classify_pre_1776_row(&constellation)?,
            Pre1776Row::Pending { .. }
        ) {
            remaining_pending = remaining_pending.saturating_add(1);
        }
    }
    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    write_stdout_line(
        &mut stdout,
        format_args!(
            "migrate_readback readonly_seq={readback_seq} base_rows_before={before_base_row_count} base_rows_after={after_base_row_count} remaining_pending={remaining_pending} unmigrated={skipped}"
        ),
    )?;
    if after_base_row_count != before_base_row_count {
        return Err(format!(
            "SYNAPSE_MIGRATE_PRE_1776_BASE_ROW_COUNT_CHANGED: Base held {before_base_row_count} \
             rows before the migration and {after_base_row_count} after. The migration only \
             overwrites Base rows in place, so any change means a row was created or lost \
             (issue #1878)"
        )
        .into());
    }
    if remaining_pending != skipped {
        return Err(format!(
            "SYNAPSE_MIGRATE_PRE_1776_READBACK_INCOMPLETE: {remaining_pending} pre-#1776 \
             constellation(s) still declare slot ids outside their panel's block after the \
             migration committed, but only {skipped} were explicitly left unmigrated \
             (issue #1878)"
        )
        .into());
    }
    Ok(())
}

/// How a migration run is allowed to deviate from "move every pending row".
struct MigrationOptions {
    /// Continue a run that a previous invocation left half finished.
    resume: bool,
    /// Leave rows that cannot reproduce their own Base row exactly where they
    /// are, naming each one, instead of stopping the whole run on the first.
    skip_unreproducible: bool,
}

/// What one constellation's move actually did, for the audit line.
struct MigrationReport {
    panel_version: u32,
    old_slot_ids: Vec<SlotId>,
    new_slot_ids: Vec<SlotId>,
    slots: Vec<SlotMoveRecord>,
    slot_rows: u64,
    raw_rows: u64,
    base_value_len_bytes: usize,
    base_value_sha256: String,
    commit_seq: u64,
}

/// The auditable digest of one moved slot row.
struct SlotMoveRecord {
    old: SlotId,
    new: SlotId,
    value_len_bytes: usize,
    value_sha256: String,
    raw_len_bytes: Option<usize>,
    raw_sha256: Option<String>,
}

/// Moves one constellation's vectors into its panel's block in a single atomic
/// batch, verifying byte identity before and after the write.
///
/// The load-bearing detail is that `decode_constellation_base` returns
/// `SlotVector::Absent` PLACEHOLDERS, while `encode_constellation_base`
/// recomputes each slot hash from the actual vector and embeds an identity hash
/// over the `(slot_id, slot_hash)` pairs. Decoding, remapping and re-encoding
/// naively would therefore write a Base row whose hashes cover Absent vectors
/// and silently corrupt every migrated row. So each slot CF value is read,
/// decoded into the real `SlotVector`, and re-encoded back to bytes that must
/// equal what was stored; then the whole Base row is re-encoded under the
/// ORIGINAL slot ids and must equal the stored Base row byte for byte. Only a
/// row that reproduces itself exactly is allowed to be rewritten.
#[allow(
    clippy::too_many_lines,
    reason = "the read, the two byte-identity proofs, the atomic batch and the post-commit verification belong to one indivisible per-constellation step"
)]
fn migrate_one_constellation(
    vault: &SynapseCalyxVault,
    candidate: &MigrationCandidate,
) -> Result<MigrationReport, Box<dyn Error>> {
    let snapshot = vault.latest_seq();
    let cx_id = candidate.cx_id;
    let key = slot_key(cx_id);
    let block = candidate.block;
    let panel = candidate.panel;

    let width = usize::from(block.width());
    if candidate.old_slot_ids.len() != width {
        return Err(format!(
            "SYNAPSE_MIGRATE_PRE_1776_SLOT_COUNT_MISMATCH: cx_id={cx_id} panel={panel} declares \
             {} slot id(s) ({}) but its exclusive block {}..={} is {width} wide, so the ordinal \
             old->new mapping is ambiguous. Resolve this row by hand before re-running \
             (issue #1878)",
            candidate.old_slot_ids.len(),
            join_slot_ids(&candidate.old_slot_ids),
            block.first,
            block.last,
        )
        .into());
    }

    let declared = read_declared_slots(vault, snapshot, cx_id, panel, &candidate.old_slot_ids)?;
    let mut moves = Vec::with_capacity(width);
    for (ordinal, slot) in declared.into_iter().enumerate() {
        let offset = u16::try_from(ordinal).map_err(|error| {
            format!(
                "SYNAPSE_MIGRATE_PRE_1776_ORDINAL_OVERFLOW: cx_id={cx_id} panel={panel}: {error}"
            )
        })?;
        let old = slot.slot;
        let new = SlotId::new(block.first.saturating_add(offset));
        if !block.contains(new.get()) || block.contains(old.get()) {
            return Err(format!(
                "SYNAPSE_MIGRATE_PRE_1776_MAPPING_OUT_OF_BLOCK: cx_id={cx_id} panel={panel} \
                 mapped slot {old} to {new}, which does not move it from outside block {}..={} to \
                 inside it (issue #1878)",
                block.first, block.last
            )
            .into());
        }
        assert_destination_free(
            vault,
            snapshot,
            cx_id,
            panel,
            new,
            &slot.quantized,
            slot.raw.as_deref(),
        )?;
        moves.push(SlotMove {
            old,
            new,
            quantized: slot.quantized,
            raw: slot.raw,
            vector: slot.vector,
        });
    }

    // Proof that this row reproduces itself: the same decode/encode round trip
    // the migration is about to perform, run with the ORIGINAL slot ids, must
    // return the stored Base row byte for byte. Anything else — a scalar, a
    // metadata entry, an anchor, the provenance hash — that did not survive the
    // round trip would show up here, before a single byte is written.
    let declared = moves
        .iter()
        .map(|slot_move| (slot_move.old, slot_move.vector.clone()))
        .collect::<BTreeMap<_, _>>();
    let (mut constellation, reproduced) =
        reproduce_stored_base(cx_id, panel, &candidate.base_value, declared)?;
    if reproduced != candidate.base_value {
        return Err(format!(
            "SYNAPSE_MIGRATE_PRE_1776_BASE_REENCODE_MISMATCH: cx_id={cx_id} panel={panel} \
             re-encodes to {} bytes (sha256={}) but is stored as {} bytes (sha256={}). This row \
             cannot be rewritten without changing content the migration must preserve. Run \
             --check-base-roundtrip for the structural diff (issue #1878)",
            reproduced.len(),
            sha256_hex(&reproduced),
            candidate.base_value.len(),
            sha256_hex(&candidate.base_value),
        )
        .into());
    }

    constellation.slots = moves
        .iter()
        .map(|slot_move| (slot_move.new, slot_move.vector.clone()))
        .collect();
    let new_base_value = encode_constellation_base(&constellation).map_err(|error| {
        format!(
            "SYNAPSE_MIGRATE_PRE_1776_REMAPPED_BASE_ENCODE_FAILED: cx_id={cx_id} panel={panel}: \
             {error}"
        )
    })?;

    let tombstone = tombstone_value();
    let mut writes = Vec::new();
    let mut raw_rows = 0_u64;
    for slot_move in &moves {
        writes.push(SynapseCalyxCfWrite::new(
            ColumnFamily::slot(slot_move.new),
            key.clone(),
            slot_move.quantized.clone(),
        ));
        if let Some(raw) = &slot_move.raw {
            raw_rows = raw_rows.saturating_add(1);
            writes.push(SynapseCalyxCfWrite::new(
                ColumnFamily::slot_raw(slot_move.new),
                key.clone(),
                raw.clone(),
            ));
            writes.push(SynapseCalyxCfWrite::new(
                ColumnFamily::slot_raw(slot_move.old),
                key.clone(),
                tombstone.clone(),
            ));
        }
        writes.push(SynapseCalyxCfWrite::new(
            ColumnFamily::slot(slot_move.old),
            key.clone(),
            tombstone.clone(),
        ));
    }
    writes.push(SynapseCalyxCfWrite::new(
        ColumnFamily::Base,
        base_key(cx_id),
        new_base_value.clone(),
    ));
    let commit_seq = vault.write_cf_batch(writes)?;

    verify_migrated_constellation(vault, cx_id, panel, &moves, &new_base_value)?;

    Ok(MigrationReport {
        panel_version: constellation.panel_version,
        old_slot_ids: moves.iter().map(|slot_move| slot_move.old).collect(),
        new_slot_ids: moves.iter().map(|slot_move| slot_move.new).collect(),
        slots: moves
            .iter()
            .map(|slot_move| SlotMoveRecord {
                old: slot_move.old,
                new: slot_move.new,
                value_len_bytes: slot_move.quantized.len(),
                value_sha256: sha256_hex(&slot_move.quantized),
                raw_len_bytes: slot_move.raw.as_ref().map(Vec::len),
                raw_sha256: slot_move.raw.as_deref().map(sha256_hex),
            })
            .collect(),
        slot_rows: u64::try_from(moves.len()).unwrap_or(u64::MAX),
        raw_rows,
        base_value_len_bytes: new_base_value.len(),
        base_value_sha256: sha256_hex(&new_base_value),
        commit_seq,
    })
}

/// Refuses to overwrite a destination slot row that already holds something
/// else.
///
/// A destination holding the identical bytes is the resumable case — the same
/// move already committed — and is allowed through. Anything else means two
/// different vectors claim one `(cf, key)`, which is exactly the collision this
/// migration exists to end.
fn assert_destination_free(
    vault: &SynapseCalyxVault,
    snapshot: u64,
    cx_id: CxId,
    panel: &str,
    new: SlotId,
    quantized: &[u8],
    raw: Option<&[u8]>,
) -> Result<(), Box<dyn Error>> {
    let key = slot_key(cx_id);
    for (cf, expected) in [
        (ColumnFamily::slot(new), Some(quantized)),
        (ColumnFamily::slot_raw(new), raw),
    ] {
        let Some(existing) = vault.read_cf_at(snapshot, cf, &key)? else {
            continue;
        };
        if expected == Some(existing.as_slice()) {
            continue;
        }
        return Err(format!(
            "SYNAPSE_MIGRATE_PRE_1776_DESTINATION_OCCUPIED: cx_id={cx_id} panel={panel} would move \
             a vector into cf/{} but that row already holds a different {}-byte value \
             (sha256={}). Refusing to overwrite it (issue #1878)",
            cf.name(),
            existing.len(),
            sha256_hex(&existing),
        )
        .into());
    }
    Ok(())
}

/// Reads back everything the committed batch claimed to do, before the run is
/// allowed to move on to the next constellation.
fn verify_migrated_constellation(
    vault: &SynapseCalyxVault,
    cx_id: CxId,
    panel: &str,
    moves: &[SlotMove],
    new_base_value: &[u8],
) -> Result<(), Box<dyn Error>> {
    let snapshot = vault.latest_seq();
    let key = slot_key(cx_id);
    for slot_move in moves {
        expect_cf_value(
            vault,
            snapshot,
            cx_id,
            panel,
            ColumnFamily::slot(slot_move.new),
            &key,
            Some(&slot_move.quantized),
        )?;
        expect_cf_value(
            vault,
            snapshot,
            cx_id,
            panel,
            ColumnFamily::slot_raw(slot_move.new),
            &key,
            slot_move.raw.as_deref(),
        )?;
        expect_cf_value(
            vault,
            snapshot,
            cx_id,
            panel,
            ColumnFamily::slot(slot_move.old),
            &key,
            None,
        )?;
        expect_cf_value(
            vault,
            snapshot,
            cx_id,
            panel,
            ColumnFamily::slot_raw(slot_move.old),
            &key,
            None,
        )?;
    }
    expect_cf_value(
        vault,
        snapshot,
        cx_id,
        panel,
        ColumnFamily::Base,
        &base_key(cx_id),
        Some(new_base_value),
    )
}

fn expect_cf_value(
    vault: &SynapseCalyxVault,
    snapshot: u64,
    cx_id: CxId,
    panel: &str,
    cf: ColumnFamily,
    key: &[u8],
    expected: Option<&[u8]>,
) -> Result<(), Box<dyn Error>> {
    let actual = vault.read_cf_at(snapshot, cf, key)?;
    if actual.as_deref() == expected {
        return Ok(());
    }
    Err(format!(
        "SYNAPSE_MIGRATE_PRE_1776_POST_COMMIT_VERIFY_FAILED: cx_id={cx_id} panel={panel} cf/{} \
         reads back as {} after the migration batch committed, but the batch wrote {} \
         (issue #1878)",
        cf.name(),
        describe_cf_value(actual.as_deref()),
        describe_cf_value(expected),
    )
    .into())
}

fn describe_cf_value(value: Option<&[u8]>) -> String {
    value.map_or_else(
        || "absent".to_owned(),
        |value| format!("{} bytes (sha256={})", value.len(), sha256_hex(value)),
    )
}

/// Classifies one Base row against the #1776 slot allocation.
///
/// The rows to migrate are exactly those carrying a panel version in
/// `SYN_PRE_1776_PANEL_VERSIONS`, so the version — not a slot-id heuristic — is
/// what selects them. A row whose panel metadata disagrees with the version it
/// carries, or whose panel has no declared block, stops the run rather than
/// being guessed at.
fn classify_pre_1776_row(constellation: &Constellation) -> Result<Pre1776Row, Box<dyn Error>> {
    let Some(&(expected_panel, _)) = SYN_PRE_1776_PANEL_VERSIONS
        .iter()
        .find(|(_, version)| *version == constellation.panel_version)
    else {
        return Ok(Pre1776Row::NotSuperseded);
    };
    let cx_id = constellation.cx_id;
    let Some(panel) = constellation.metadata.get(META_PANEL_NAME) else {
        return Err(format!(
            "SYNAPSE_MIGRATE_PRE_1776_PANEL_UNLABELLED: cx_id={cx_id} carries superseded \
             panel_version={} but has no {META_PANEL_NAME} metadata, so its panel block cannot be \
             resolved (issue #1878)",
            constellation.panel_version
        )
        .into());
    };
    if panel.as_str() != expected_panel {
        return Err(format!(
            "SYNAPSE_MIGRATE_PRE_1776_PANEL_NAME_MISMATCH: cx_id={cx_id} carries superseded \
             panel_version={} which belongs to panel={expected_panel}, but its {META_PANEL_NAME} \
             metadata says panel={panel} (issue #1878)",
            constellation.panel_version
        )
        .into());
    }
    let Some(block) = PRE_1776_PANEL_BLOCKS
        .iter()
        .find(|block| block.panel == expected_panel)
    else {
        return Err(format!(
            "SYNAPSE_MIGRATE_PRE_1776_BLOCK_MISSING: cx_id={cx_id} panel={expected_panel} has no \
             entry in PRE_1776_PANEL_BLOCKS in this example (issue #1878)"
        )
        .into());
    };
    let slot_ids = constellation.slots.keys().copied().collect::<Vec<_>>();
    if slot_ids.is_empty() {
        return Ok(Pre1776Row::NoSlots);
    }
    let inside = slot_ids
        .iter()
        .filter(|slot| block.contains(slot.get()))
        .count();
    if inside == slot_ids.len() {
        return Ok(Pre1776Row::AlreadyInBlock);
    }
    if inside != 0 {
        return Err(format!(
            "SYNAPSE_MIGRATE_PRE_1776_MIXED_BLOCK_STATE: cx_id={cx_id} panel={expected_panel} \
             declares slot ids {} of which {inside} are inside block {}..={} and the rest are \
             outside. Each constellation moves in one atomic batch, so this half-moved shape \
             cannot be produced by an interrupted run and must be resolved by hand (issue #1878)",
            join_slot_ids(&slot_ids),
            block.first,
            block.last,
        )
        .into());
    }
    Ok(Pre1776Row::Pending {
        block,
        old_slot_ids: slot_ids,
    })
}

/// Fails closed when this example's block table does not cover every superseded
/// panel version the storage crate declares.
fn verify_pre_1776_block_table() -> Result<(), Box<dyn Error>> {
    for (panel, version) in SYN_PRE_1776_PANEL_VERSIONS {
        if !PRE_1776_PANEL_BLOCKS
            .iter()
            .any(|block| block.panel == *panel)
        {
            return Err(format!(
                "SYNAPSE_MIGRATE_PRE_1776_BLOCK_TABLE_INCOMPLETE: SYN_PRE_1776_PANEL_VERSIONS \
                 declares panel={panel} version={version}, but PRE_1776_PANEL_BLOCKS in \
                 crates/synapse-storage/examples/dump_cf.rs has no block for it. Add its block \
                 from PANEL_SLOT_BLOCKS before migrating (issue #1878)"
            )
            .into());
        }
    }
    Ok(())
}

/// Reads one raw CF row, whichever kind of vault handle is open.
///
/// The preflight audit runs read-only so it can be pointed at the live vault
/// while the daemon owns it; the migration holds the writer lock. Both need the
/// identical read/decode/re-encode path, and a path that differs between the
/// dry run and the real run would make the dry run worthless.
trait CfReader {
    fn read_cf(
        &self,
        snapshot: u64,
        cf: ColumnFamily,
        key: &[u8],
    ) -> Result<Option<Vec<u8>>, Box<dyn Error>>;
}

impl CfReader for SynapseCalyxVault {
    fn read_cf(
        &self,
        snapshot: u64,
        cf: ColumnFamily,
        key: &[u8],
    ) -> Result<Option<Vec<u8>>, Box<dyn Error>> {
        Ok(self.read_cf_at(snapshot, cf, key)?)
    }
}

impl CfReader for SynapseCalyxReadOnlyVault {
    fn read_cf(
        &self,
        snapshot: u64,
        cf: ColumnFamily,
        key: &[u8],
    ) -> Result<Option<Vec<u8>>, Box<dyn Error>> {
        Ok(self.read_cf_at(snapshot, cf, key)?)
    }
}

/// One physical slot row behind a Base row's declared slot id.
struct DeclaredSlot {
    slot: SlotId,
    quantized: Vec<u8>,
    raw: Option<Vec<u8>>,
    vector: SlotVector,
}

/// Reads the physical vectors a Base row declares and proves each one
/// re-encodes to exactly the bytes stored in its slot CF.
///
/// The Base row's per-slot hash is `blake3` over precisely these bytes
/// (`vault/prepared.rs` hashes the same buffer it writes to `cf/slot_<id>`), so
/// a vector that does not re-encode byte for byte cannot reproduce its own Base
/// row either.
fn read_declared_slots(
    reader: &impl CfReader,
    snapshot: u64,
    cx_id: CxId,
    panel: &str,
    slot_ids: &[SlotId],
) -> Result<Vec<DeclaredSlot>, Box<dyn Error>> {
    let key = slot_key(cx_id);
    let mut declared = Vec::with_capacity(slot_ids.len());
    for &slot in slot_ids {
        let Some(quantized) = reader.read_cf(snapshot, ColumnFamily::slot(slot), &key)? else {
            return Err(format!(
                "SYNAPSE_MIGRATE_PRE_1776_SLOT_ROW_MISSING: cx_id={cx_id} panel={panel} declares \
                 slot {slot} in its Base row but cf/{} holds no row for it, so its vector cannot \
                 be moved and its Base row cannot be reproduced (issue #1878)",
                ColumnFamily::slot(slot).name()
            )
            .into());
        };
        let vector = decode_slot_vector(&quantized).map_err(|error| {
            format!(
                "SYNAPSE_MIGRATE_PRE_1776_SLOT_DECODE_FAILED: cx_id={cx_id} panel={panel} \
                 slot={slot} value_len_bytes={} value_sha256={}: {error}",
                quantized.len(),
                sha256_hex(&quantized)
            )
        })?;
        let reencoded = encode_slot_vector(&vector).map_err(|error| {
            format!(
                "SYNAPSE_MIGRATE_PRE_1776_SLOT_REENCODE_FAILED: cx_id={cx_id} panel={panel} \
                 slot={slot}: {error}"
            )
        })?;
        if reencoded != quantized {
            return Err(format!(
                "SYNAPSE_MIGRATE_PRE_1776_SLOT_REENCODE_MISMATCH: cx_id={cx_id} panel={panel} \
                 slot={slot} re-encodes to {} bytes (sha256={}) but is stored as {} bytes \
                 (sha256={}). The Base row's slot hashes are computed from the encoded vector, so \
                 rewriting this row would change what its hashes cover (issue #1878)",
                reencoded.len(),
                sha256_hex(&reencoded),
                quantized.len(),
                sha256_hex(&quantized),
            )
            .into());
        }
        let raw = reader.read_cf(snapshot, ColumnFamily::slot_raw(slot), &key)?;
        declared.push(DeclaredSlot {
            slot,
            quantized,
            raw,
            vector,
        });
    }
    Ok(declared)
}

/// Re-encodes a stored Base row from its real vectors under its ORIGINAL slot
/// ids, returning the decoded constellation and the reproduced bytes.
///
/// The caller compares. Equality is the licence to rewrite the row; inequality
/// is a finding, never something to work around.
fn reproduce_stored_base(
    cx_id: CxId,
    panel: &str,
    stored: &[u8],
    slots: BTreeMap<SlotId, SlotVector>,
) -> Result<(Constellation, Vec<u8>), Box<dyn Error>> {
    let mut constellation = decode_base_for_migration(&base_key(cx_id), stored)?;
    constellation.slots = slots;
    let reproduced = encode_constellation_base(&constellation).map_err(|error| {
        format!(
            "SYNAPSE_MIGRATE_PRE_1776_BASE_REENCODE_FAILED: cx_id={cx_id} panel={panel}: {error}"
        )
    })?;
    Ok((constellation, reproduced))
}

fn decode_base_for_migration(key: &[u8], value: &[u8]) -> Result<Constellation, Box<dyn Error>> {
    decode_constellation_base(value).map_err(|error| {
        format!(
            "SYNAPSE_MIGRATE_PRE_1776_BASE_DECODE_FAILED key_hex={} value_len_bytes={} \
             value_sha256={}: {error}",
            hex_encode(key),
            value.len(),
            sha256_hex(value)
        )
        .into()
    })
}

/// Read-only preflight for the #1878 migration: how many superseded rows can
/// reproduce their own Base row, and for the ones that cannot, exactly which
/// bytes differ.
///
/// The migration refuses to rewrite a Base row it cannot reproduce byte for
/// byte, so this is the number that decides whether the migration can move all
/// of the superseded rows or only some. It writes nothing and does not take the
/// writer lock, so it can be pointed at the live vault.
#[allow(
    clippy::too_many_lines,
    clippy::needless_pass_by_value,
    reason = "the preflight owns its path and prints the whole scan, per-failure structural diff and per-panel tally inline"
)]
fn check_base_roundtrip(db_path: PathBuf) -> Result<(), Box<dyn Error>> {
    verify_pre_1776_block_table()?;
    let vault = SynapseCalyxReadOnlyVault::open_existing_with_cfs(
        SynapseCalyxConfig::from_vault_dir(db_path.clone()),
        None,
    )?;
    let snapshot = vault.latest_seq();
    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    write_stdout_line(
        &mut stdout,
        format_args!(
            "check_base_roundtrip db_path={} mode=read_only vault_id={} snapshot={snapshot}",
            db_path.display(),
            vault.vault_id(),
        ),
    )?;

    // panel -> (pass, fail)
    let mut tally: BTreeMap<String, (u64, u64)> = BTreeMap::new();
    let mut checked = 0_u64;
    let mut failures = 0_u64;
    for (key, value) in vault.scan_cf_at(snapshot, ColumnFamily::Base)? {
        let constellation = decode_base_for_migration(&key, &value)?;
        let (panel, slot_ids, migrated) = match classify_pre_1776_row(&constellation)? {
            Pre1776Row::NotSuperseded | Pre1776Row::NoSlots => continue,
            Pre1776Row::AlreadyInBlock => (
                constellation
                    .metadata
                    .get(META_PANEL_NAME)
                    .cloned()
                    .unwrap_or_default(),
                constellation.slots.keys().copied().collect::<Vec<_>>(),
                true,
            ),
            Pre1776Row::Pending {
                block,
                old_slot_ids,
            } => (block.panel.to_owned(), old_slot_ids, false),
        };
        let cx_id = constellation.cx_id;
        checked = checked.saturating_add(1);
        let entry = tally.entry(panel.clone()).or_default();

        let outcome =
            read_declared_slots(&vault, snapshot, cx_id, &panel, &slot_ids).and_then(|declared| {
                let slots = declared
                    .into_iter()
                    .map(|slot| (slot.slot, slot.vector))
                    .collect::<BTreeMap<_, _>>();
                reproduce_stored_base(cx_id, &panel, &value, slots)
            });
        match outcome {
            Ok((_, reproduced)) if reproduced == value => {
                entry.0 = entry.0.saturating_add(1);
            }
            Ok((decoded, reproduced)) => {
                entry.1 = entry.1.saturating_add(1);
                failures = failures.saturating_add(1);
                write_stdout_line(
                    &mut stdout,
                    format_args!(
                        "roundtrip_fail cx_id={cx_id} panel={panel} panel_version={} already_in_block={migrated} slot_ids={} stored_len_bytes={} stored_sha256={} reencoded_len_bytes={} reencoded_sha256={}",
                        decoded.panel_version,
                        join_slot_ids(&slot_ids),
                        value.len(),
                        sha256_hex(&value),
                        reproduced.len(),
                        sha256_hex(&reproduced),
                    ),
                )?;
                for region in classify_base_diff(&value, &reproduced, &decoded) {
                    write_stdout_line(
                        &mut stdout,
                        format_args!("roundtrip_diff cx_id={cx_id} panel={panel} {region}"),
                    )?;
                }
            }
            Err(error) => {
                entry.1 = entry.1.saturating_add(1);
                failures = failures.saturating_add(1);
                write_stdout_line(
                    &mut stdout,
                    format_args!("roundtrip_fail cx_id={cx_id} panel={panel} error={error}"),
                )?;
            }
        }
    }

    for (panel, (pass, fail)) in &tally {
        write_stdout_line(
            &mut stdout,
            format_args!(
                "roundtrip_panel panel={panel} checked={} pass={pass} fail={fail}",
                pass.saturating_add(*fail)
            ),
        )?;
    }
    write_stdout_line(
        &mut stdout,
        format_args!(
            "roundtrip_summary checked={checked} pass={} fail={failures}",
            checked.saturating_sub(failures)
        ),
    )?;
    Ok(())
}

/// Names the structural region of every byte range in which a stored Base row
/// and its re-encoding disagree.
///
/// The layout is fixed by `encode_constellation_base`: a 102-byte header, a
/// 32-byte identity hash, the input-ref tail, then the `(slot_id, slot_hash)`
/// map. `decode_constellation_base` skips the identity hash and every slot
/// hash, which is why a difference confined to those regions is invisible to a
/// decode-and-compare and shows up only as a byte diff of equal length.
fn classify_base_diff(
    stored: &[u8],
    reproduced: &[u8],
    constellation: &Constellation,
) -> Vec<String> {
    let mut regions = Vec::new();
    if stored.len() != reproduced.len() {
        regions.push(format!(
            "region=length stored_len_bytes={} reencoded_len_bytes={}",
            stored.len(),
            reproduced.len()
        ));
        return regions;
    }
    let pointer_len = constellation
        .input_ref
        .pointer
        .as_ref()
        .map_or(0, |pointer| 4 + pointer.len());
    let slot_map = HEADER_LEN + IDENTITY_HASH_LEN + 2 + pointer_len;
    let slot_ids = constellation.slots.keys().copied().collect::<Vec<_>>();
    for (start, end) in differing_ranges(stored, reproduced) {
        let label = base_region_label(start, slot_map, &slot_ids);
        regions.push(format!(
            "region={label} offset={start} len={} stored_hex={} reencoded_hex={}",
            end - start,
            hex_encode(&stored[start..end]),
            hex_encode(&reproduced[start..end]),
        ));
    }
    regions
}

/// Maps a byte offset to the field of the Base encoding that contains it.
fn base_region_label(offset: usize, slot_map: usize, slot_ids: &[SlotId]) -> String {
    if offset < HEADER_LEN {
        return "header".to_owned();
    }
    if offset < HEADER_LEN + IDENTITY_HASH_LEN {
        return "identity_hash".to_owned();
    }
    if offset < slot_map {
        return "input_ref_tail".to_owned();
    }
    if offset < slot_map + 2 {
        return "slot_count".to_owned();
    }
    let entry = (offset - slot_map - 2) / SLOT_MAP_ENTRY_LEN;
    let within = (offset - slot_map - 2) % SLOT_MAP_ENTRY_LEN;
    let Some(slot) = slot_ids.get(entry) else {
        return format!("tail_after_slot_map entry_index={entry}");
    };
    if within < 2 {
        format!("slot_map_id slot_id={slot}")
    } else {
        format!("slot_map_hash slot_id={slot}")
    }
}

/// Maximal half-open ranges in which two equal-length buffers differ.
fn differing_ranges(left: &[u8], right: &[u8]) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let mut start = None;
    for (index, (a, b)) in left.iter().zip(right.iter()).enumerate() {
        match (a == b, start) {
            (false, None) => start = Some(index),
            (true, Some(from)) => {
                ranges.push((from, index));
                start = None;
            }
            _ => {}
        }
    }
    if let Some(from) = start {
        ranges.push((from, left.len()));
    }
    ranges
}

fn join_slot_ids(slot_ids: &[SlotId]) -> String {
    slot_ids
        .iter()
        .map(|slot| slot.get().to_string())
        .collect::<Vec<_>>()
        .join(",")
}

#[allow(
    clippy::too_many_lines,
    clippy::needless_pass_by_value,
    reason = "native constellation dump prints a complete physical readback in deterministic order"
)]
fn dump_native_cx(
    db_path: PathBuf,
    cx_id: &str,
    reveal_metadata: bool,
) -> Result<(), Box<dyn Error>> {
    let cx_id = CxId::from_str(cx_id)?;
    let vault = SynapseCalyxReadOnlyVault::open_existing_with_cfs(
        SynapseCalyxConfig::from_vault_dir(db_path.clone()),
        None,
    )?;
    let snapshot = vault.latest_seq();
    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    if !write_stdout_line(
        &mut stdout,
        format_args!(
            "native_cx db_path={} mode=read_only vault_id={} snapshot={} cx_id={}",
            db_path.display(),
            vault.vault_id(),
            snapshot,
            cx_id
        ),
    )? {
        return Ok(());
    }

    let base = vault.read_cf_at(snapshot, ColumnFamily::Base, &base_key(cx_id))?;
    let Some(base_bytes) = base else {
        write_stdout_line(
            &mut stdout,
            format_args!(
                "base present=false key_hex={}",
                hex_encode(&base_key(cx_id))
            ),
        )?;
        return Ok(());
    };
    let constellation = decode_constellation_base(&base_bytes)?;
    if !write_stdout_line(
        &mut stdout,
        format_args!(
            "base present=true key_hex={} value_len_bytes={} value_sha256={} panel_version={} created_at={} input_hash={} input_pointer={} redacted={} slot_count={} scalar_count={} metadata_count={} ledger_seq={}",
            hex_encode(&base_key(cx_id)),
            base_bytes.len(),
            sha256_hex(&base_bytes),
            constellation.panel_version,
            constellation.created_at,
            hex_encode(&constellation.input_ref.hash),
            constellation.input_ref.pointer.as_deref().unwrap_or(""),
            constellation.input_ref.redacted,
            constellation.slots.len(),
            constellation.scalars.len(),
            constellation.metadata.len(),
            constellation.provenance.seq,
        ),
    )? {
        return Ok(());
    }

    for (name, value) in &constellation.scalars {
        if !write_stdout_line(
            &mut stdout,
            format_args!(
                "base_scalar name={} value={} bits=0x{:016x}",
                name,
                value,
                value.to_bits()
            ),
        )? {
            return Ok(());
        }
    }

    for (key, value) in &constellation.metadata {
        let rendered = if reveal_metadata {
            value.clone()
        } else {
            format!(
                "sha256:{} len={}",
                sha256_hex(value.as_bytes()),
                value.len()
            )
        };
        if !write_stdout_line(
            &mut stdout,
            format_args!("metadata key={key} value={rendered}"),
        )? {
            return Ok(());
        }
    }

    for slot in constellation.slots.keys() {
        let key = slot_key(cx_id);
        let value = vault.read_cf_at(snapshot, ColumnFamily::slot(*slot), &key)?;
        match value {
            Some(value) => {
                let shape = inspect_slot_vector(&value)?;
                if !write_stdout_line(
                    &mut stdout,
                    format_args!(
                        "slot slot_id={} present=true key_hex={} value_len_bytes={} value_sha256={} shape={:?}",
                        slot,
                        hex_encode(&key),
                        value.len(),
                        sha256_hex(&value),
                        shape,
                    ),
                )? {
                    return Ok(());
                }
            }
            None => {
                if !write_stdout_line(
                    &mut stdout,
                    format_args!(
                        "slot slot_id={} present=false key_hex={}",
                        slot,
                        hex_encode(&key)
                    ),
                )? {
                    return Ok(());
                }
            }
        }
    }

    let scalar_name_by_id = constellation
        .scalars
        .keys()
        .map(|name| (scalar_id_for_key(name).get(), name.as_str()))
        .collect::<BTreeMap<_, _>>();
    let scalar_rows = vault.scan_cf_at(snapshot, ColumnFamily::Scalars)?;
    let cx_suffix = cx_id.as_bytes();
    let mut physical_scalar_rows = 0_u64;
    for (key, value) in scalar_rows {
        if key.len() != 4 + cx_suffix.len() || &key[4..] != cx_suffix {
            continue;
        }
        physical_scalar_rows = physical_scalar_rows.saturating_add(1);
        let scalar_id = u32::from_be_bytes([key[0], key[1], key[2], key[3]]);
        let scalar_name = scalar_name_by_id
            .get(&scalar_id)
            .copied()
            .unwrap_or("<unknown>");
        let value_bits = if value.len() == 8 {
            u64::from_be_bytes([
                value[0], value[1], value[2], value[3], value[4], value[5], value[6], value[7],
            ])
        } else {
            0
        };
        let value_text = if value.len() == 8 {
            f64::from_bits(value_bits).to_string()
        } else {
            "<invalid-len>".to_owned()
        };
        if !write_stdout_line(
            &mut stdout,
            format_args!(
                "scalar_cf name={} scalar_id=0x{:08x} key_hex={} value_len_bytes={} value={} bits=0x{:016x}",
                scalar_name,
                scalar_id,
                hex_encode(&key),
                value.len(),
                value_text,
                value_bits,
            ),
        )? {
            return Ok(());
        }
    }
    write_stdout_line(
        &mut stdout,
        format_args!("scalar_cf_rows_for_cx={physical_scalar_rows}"),
    )?;
    Ok(())
}

fn canonical_hex(value: &str) -> Result<String, Box<dyn Error>> {
    let value = value.strip_prefix("0x").unwrap_or(value);
    if !value.len().is_multiple_of(2) {
        return Err(format!("hex input must have an even number of digits: {value:?}").into());
    }
    let value = value.to_ascii_lowercase();
    for index in (0..value.len()).step_by(2) {
        u8::from_str_radix(&value[index..index + 2], 16)
            .map_err(|error| format!("invalid hex byte at offset {index}: {error}"))?;
    }
    Ok(value)
}

/// Drives the ensemble capability card over a real panel and prints every pair
/// row's gain evidence: the reported gain, the **unclamped** raw gain, whether
/// the data-processing-inequality floor moved it, and the instrument behind each
/// of the three terms (#1942).
///
/// Takes the writer lock — the pass persists an `EnsembleCard` Assay row as
/// durable evidence that it ran — so point it at a vault copy, not the live
/// vault the daemon owns.
#[allow(
    clippy::too_many_lines,
    clippy::needless_pass_by_value,
    reason = "the FSV driver owns its path and prints the whole card inline"
)]
fn ensemble_card_report(
    db_path: PathBuf,
    panel_version: u32,
    anchor_kind: &str,
    max_records: usize,
    min_gate_lenses: usize,
) -> Result<(), Box<dyn Error>> {
    let vault = SynapseCalyxVault::open(SynapseCalyxConfig::from_vault_dir(db_path.clone()))?;
    let snapshot = vault.latest_seq();
    let assay_rows_before = vault.scan_cf_at(snapshot, ColumnFamily::Assay)?.len();
    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    write_stdout_line(
        &mut stdout,
        format_args!(
            "ensemble_card db_path={} vault_id={} snapshot={snapshot} panel_version={panel_version} anchor_kind={anchor_kind} max_records={max_records} min_gate_lenses={min_gate_lenses} assay_rows_before={assay_rows_before}",
            db_path.display(),
            vault.vault_id(),
        ),
    )?;

    let params =
        synapse_calyx::SynapseCalyxAssayParams::new(panel_version, anchor_kind.to_string())
            .with_max_records(max_records)
            .with_lens_names(synapse_storage::constellations::syn_slot_lens_names());
    let report = vault.assay_ensemble_card(&params, min_gate_lenses)?;
    let card = &report.card;
    write_stdout_line(
        &mut stdout,
        format_args!(
            "corpus records_scanned={} anchored_records={} declared_slots={} measured_slots={:?} excluded_lenses={} assay_cf_rows={}",
            report.records_scanned,
            report.anchored_records,
            report.declared_slots,
            report.measured_slots,
            report.excluded_lenses.len(),
            report.assay_cf_rows,
        ),
    )?;
    for lens in &report.excluded_lenses {
        write_stdout_line(
            &mut stdout,
            format_args!(
                "excluded slot={} name={} reason={}",
                lens.slot, lens.name, lens.reason
            ),
        )?;
    }

    write_stdout_line(
        &mut stdout,
        format_args!(
            "card schema_version={} source={} pid_method={} panel_lens_count={} n_samples={} anchor_entropy_bits={:.6} panel_bits={:.6} n_eff={:.4} sufficient={} deficit_bits={} keep={} park={} retire={} pairs_monotonicity_floored={}",
            card.schema_version,
            card.source,
            card.pid_method,
            card.panel_lens_count,
            card.n_samples,
            card.anchor_entropy_bits,
            card.panel_bits,
            card.n_eff,
            card.sufficient,
            calyx_assay::sufficiency::format_deficit_bits(card.deficit_bits, 6),
            card.keep_count,
            card.park_count,
            card.retire_count,
            card.pairs_monotonicity_floored,
        ),
    )?;
    // The verdict is computed from the sufficiency *basis*, which is the
    // estimate's CI lower bound rather than its point value, so printing
    // `panel_bits` alone can show a card that says "not sufficient" beside a
    // panel_bits equal to the anchor entropy. Print the basis the verdict
    // actually used.
    write_stdout_line(
        &mut stdout,
        format_args!(
            "sufficiency basis_bits={:.9} panel_bits={:.9} anchor_entropy_bits={:.9} deficit_bits={} sufficient={} verdict_basis={} estimator_resolution_bits={:.3e} trust={:?}",
            card.sufficiency.sufficiency_basis_bits,
            card.sufficiency.panel_bits,
            card.anchor_entropy_bits,
            calyx_assay::sufficiency::format_deficit_bits(card.sufficiency.deficit_bits, 9),
            card.sufficiency.sufficient,
            card.sufficiency.verdict_basis.as_str(),
            card.sufficiency.estimator_resolution_bits,
            card.sufficiency.trust,
        ),
    )?;
    write_stdout_line(
        &mut stdout,
        format_args!(
            "a37 status={} families={} n_eff={:.4} floor={:.4} pair_evidence={} redundancy_bound={} no_collapse={}",
            card.a37_diversity.status,
            card.a37_diversity.association_family_count,
            card.a37_diversity.n_eff,
            card.a37_diversity.n_eff_floor,
            card.a37_diversity.pair_evidence_pass,
            card.a37_diversity.redundancy_bound_pass,
            card.a37_diversity.no_collapse_pass,
        ),
    )?;
    for lens in &card.lenses {
        write_stdout_line(
            &mut stdout,
            format_args!(
                "lens slot={} name={} solo_bits={:.6} panel_without_bits={:.6} marginal_bits={:.6} pid_unique={:.6} pid_redundant={:.6} pid_synergistic={:.6} decision={:?}",
                lens.slot,
                lens.name,
                lens.solo_bits,
                lens.panel_without_bits,
                lens.marginal_bits,
                lens.pid.unique_bits,
                lens.pid.redundant_bits,
                lens.pid.synergistic_bits,
                lens.decision,
            ),
        )?;
    }
    for pair in &card.pairs {
        write_stdout_line(
            &mut stdout,
            format_args!(
                "pair slot_a={} slot_b={} pair_bits={:.6} gain_bits={:.6} raw_gain_bits={:.6} monotonicity_floor_applied={} estimators=pair:{:?}/left:{:?}/right:{:?} corr={:.6} nmi={:.6}",
                pair.slot_a,
                pair.slot_b,
                pair.pair_bits,
                pair.synergy_gain_bits,
                pair.raw_synergy_gain_bits,
                pair.synergy_monotonicity_floor_applied,
                pair.synergy_estimators.pair,
                pair.synergy_estimators.left,
                pair.synergy_estimators.right,
                pair.corr,
                pair.nmi,
            ),
        )?;
    }

    // Every reported zero must be either a measured zero or a flagged clamp.
    let silent_zeros = card
        .pairs
        .iter()
        .filter(|pair| {
            pair.synergy_gain_bits == 0.0
                && !pair.synergy_monotonicity_floor_applied
                && pair.raw_synergy_gain_bits != 0.0
        })
        .count();
    let floored = card
        .pairs
        .iter()
        .filter(|pair| pair.synergy_monotonicity_floor_applied)
        .count();
    let after_seq = vault.latest_seq();
    let assay_rows_after = vault.scan_cf_at(after_seq, ColumnFamily::Assay)?.len();
    write_stdout_line(
        &mut stdout,
        format_args!(
            "ensemble_card_summary pairs={} floored={} card_floored_counter={} silent_zero_gains={} assay_rows_before={assay_rows_before} assay_rows_after={assay_rows_after} snapshot_after={after_seq}",
            card.pairs.len(),
            floored,
            card.pairs_monotonicity_floored,
            silent_zeros,
        ),
    )?;
    Ok(())
}

fn write_stdout_line(
    stdout: &mut impl Write,
    args: fmt::Arguments<'_>,
) -> Result<bool, Box<dyn Error>> {
    match stdout
        .write_fmt(args)
        .and_then(|()| stdout.write_all(b"\n"))
    {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => Ok(false),
        Err(error) => Err(format!(
            "DUMP_CF_STDOUT_WRITE_FAILED kind={:?}: {error}",
            error.kind()
        )
        .into()),
    }
}

/// Verdict for one declared slot of one constellation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SlotHashVerdict {
    /// The Base row's recorded hash covers the bytes actually in the slot CF.
    Covered,
    /// The Base row records a hash of something other than the stored bytes —
    /// the `Absent` placeholder left behind by an un-updated backfill (#1888).
    Mismatched,
    /// The Base row declares the slot but the slot CF has no row for it.
    SlotRowMissing,
    /// The slot CF row is compression-tagged, so its stored bytes are not the
    /// `encode_slot_vector` bytes a Base hash covers. Reported, never repaired.
    Compressed,
}

impl fmt::Display for SlotHashVerdict {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            Self::Covered => "covered",
            Self::Mismatched => "mismatched",
            Self::SlotRowMissing => "slot_row_missing",
            Self::Compressed => "compressed",
        };
        formatter.write_str(text)
    }
}

/// The compression tag `calyx-registry` writes ahead of a compressed slot
/// payload. Named here rather than depended on, so the audit keeps working even
/// if that crate leaves this binary's graph.
const COMPRESSED_SLOT_TAG: u8 = 16;

/// Counts — and optionally repairs — Base rows whose recorded slot hashes do
/// not cover the vectors actually stored in the slot CFs (issue #1888).
///
/// This is the measurement `--check-base-roundtrip` could not make: that mode
/// only visits pre-#1776 superseded rows, and only compares a re-encoding
/// against the stored bytes. This one visits **every** Base row in the vault
/// and compares each recorded slot hash against an independent hash of the
/// bytes present in `cf/slot_<id>`, which is the property the hash exists to
/// assert.
#[allow(
    clippy::too_many_lines,
    clippy::needless_pass_by_value,
    reason = "the audit owns its path and prints the whole scan, per-row verdict and per-panel tally inline"
)]
fn audit_slot_hashes(db_path: PathBuf, repair: bool) -> Result<(), Box<dyn Error>> {
    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    let config = SynapseCalyxConfig::from_vault_dir(db_path.clone());
    // A repair needs the writer lock; an audit must stay pointable at a live
    // vault, so only one of the two handles is ever opened.
    let writable = if repair {
        Some(SynapseCalyxVault::open(config.clone())?)
    } else {
        None
    };
    let read_only = if repair {
        None
    } else {
        Some(SynapseCalyxReadOnlyVault::open_existing_with_cfs(
            config, None,
        )?)
    };

    let snapshot = match (writable.as_ref(), read_only.as_ref()) {
        (Some(vault), _) => vault.latest_seq(),
        (None, Some(vault)) => vault.latest_seq(),
        (None, None) => unreachable!("exactly one vault handle is opened"),
    };
    let vault_id = match (writable.as_ref(), read_only.as_ref()) {
        (Some(vault), _) => vault.vault_id(),
        (None, Some(vault)) => vault.vault_id(),
        (None, None) => unreachable!("exactly one vault handle is opened"),
    };
    write_stdout_line(
        &mut stdout,
        format_args!(
            "audit_slot_hashes db_path={} mode={} vault_id={vault_id} snapshot={snapshot}",
            db_path.display(),
            if repair { "writeable" } else { "read_only" },
        ),
    )?;

    // panel -> (rows_checked, rows_with_at_least_one_bad_slot)
    let mut tally: BTreeMap<String, (u64, u64)> = BTreeMap::new();
    let mut rows_checked = 0_u64;
    let mut rows_bad = 0_u64;
    let mut slots_checked = 0_u64;
    let mut slots_mismatched = 0_u64;
    let mut slots_missing = 0_u64;
    let mut slots_compressed = 0_u64;
    let mut repairs = Vec::<SynapseCalyxCfWrite>::new();
    let mut repaired_rows = 0_u64;

    let base_rows = match (writable.as_ref(), read_only.as_ref()) {
        (Some(vault), _) => vault.scan_cf_at(snapshot, ColumnFamily::Base)?,
        (None, Some(vault)) => vault.scan_cf_at(snapshot, ColumnFamily::Base)?,
        (None, None) => unreachable!("exactly one vault handle is opened"),
    };
    for (key, value) in base_rows {
        let (constellation, slot_hashes) = decode_constellation_base_with_slot_hashes(&value)
            .map_err(|error| {
                format!(
                    "AUDIT_BASE_DECODE_FAILED key_hex={} value_len_bytes={} value_sha256={}: {error}",
                    hex_encode(&key),
                    value.len(),
                    sha256_hex(&value)
                )
            })?;
        if slot_hashes.is_empty() {
            continue;
        }
        let cx_id = constellation.cx_id;
        let panel = constellation
            .metadata
            .get(META_PANEL_NAME)
            .cloned()
            .unwrap_or_else(|| format!("panel_version={}", constellation.panel_version));
        rows_checked = rows_checked.saturating_add(1);
        let entry = tally.entry(panel.clone()).or_default();
        entry.0 = entry.0.saturating_add(1);

        let mut restated = BaseRowRewrite::decode(&value)?;
        let mut row_has_defect = false;
        let mut row_repairable = true;
        for (slot, recorded) in &slot_hashes {
            slots_checked = slots_checked.saturating_add(1);
            let stored = match (writable.as_ref(), read_only.as_ref()) {
                (Some(vault), _) => {
                    vault.read_cf_at(snapshot, ColumnFamily::slot(*slot), &slot_key(cx_id))?
                }
                (None, Some(vault)) => {
                    vault.read_cf_at(snapshot, ColumnFamily::slot(*slot), &slot_key(cx_id))?
                }
                (None, None) => unreachable!("exactly one vault handle is opened"),
            };
            let (verdict, actual) = match stored.as_deref() {
                None => (SlotHashVerdict::SlotRowMissing, None),
                Some(bytes) if bytes.first().copied() == Some(COMPRESSED_SLOT_TAG) => {
                    (SlotHashVerdict::Compressed, None)
                }
                Some(bytes) => {
                    let actual = hash_slot_bytes(bytes);
                    if actual == *recorded {
                        (SlotHashVerdict::Covered, Some(actual))
                    } else {
                        (SlotHashVerdict::Mismatched, Some(actual))
                    }
                }
            };
            match verdict {
                SlotHashVerdict::Covered => continue,
                SlotHashVerdict::Mismatched => {
                    slots_mismatched = slots_mismatched.saturating_add(1);
                }
                SlotHashVerdict::SlotRowMissing => {
                    slots_missing = slots_missing.saturating_add(1);
                    row_repairable = false;
                }
                SlotHashVerdict::Compressed => {
                    slots_compressed = slots_compressed.saturating_add(1);
                    row_repairable = false;
                }
            }
            row_has_defect = true;
            write_stdout_line(
                &mut stdout,
                format_args!(
                    "slot_hash_verdict cx_id={cx_id} panel={panel} slot={} verdict={verdict} recorded_blake3={} actual_blake3={} stored_len_bytes={}",
                    slot.get(),
                    hex_encode(recorded),
                    actual
                        .as_ref()
                        .map_or_else(|| "none".to_owned(), |hash| hex_encode(hash)),
                    stored
                        .as_ref()
                        .map_or_else(|| "none".to_owned(), |bytes| bytes.len().to_string()),
                ),
            )?;
            if verdict == SlotHashVerdict::Mismatched
                && let Some(bytes) = stored.as_deref()
            {
                restated.set_slot_hash(*slot, bytes)?;
            }
        }
        if !row_has_defect {
            continue;
        }
        rows_bad = rows_bad.saturating_add(1);
        entry.1 = entry.1.saturating_add(1);
        if repair {
            if !row_repairable {
                // Fail closed rather than write a partially-correct integrity
                // record: a missing or compression-tagged slot row means the
                // audit cannot state what the hash should be.
                return Err(format!(
                    "AUDIT_SLOT_HASH_UNREPAIRABLE cx_id={cx_id} panel={panel}: a declared slot is missing from its slot CF or is compression-tagged, so its Base hash cannot be restated; resolve that row before repairing"
                )
                .into());
            }
            repairs.push(SynapseCalyxCfWrite::new(
                ColumnFamily::Base,
                key.clone(),
                restated.encode()?,
            ));
            repaired_rows = repaired_rows.saturating_add(1);
        }
    }

    if repair && !repairs.is_empty() {
        let vault = writable
            .as_ref()
            .ok_or("repair requested without a writable vault handle")?;
        let commit_seq = vault.write_cf_batch(repairs)?;
        write_stdout_line(
            &mut stdout,
            format_args!(
                "audit_slot_hashes_repair repaired_rows={repaired_rows} commit_seq={commit_seq}"
            ),
        )?;
    }

    for (panel, (checked, bad)) in &tally {
        write_stdout_line(
            &mut stdout,
            format_args!("slot_hash_panel panel={panel} rows_checked={checked} rows_bad={bad}"),
        )?;
    }
    write_stdout_line(
        &mut stdout,
        format_args!(
            "audit_slot_hashes_summary rows_checked={rows_checked} rows_bad={rows_bad} slots_checked={slots_checked} slots_mismatched={slots_mismatched} slots_missing={slots_missing} slots_compressed={slots_compressed} repaired_rows={repaired_rows}"
        ),
    )?;
    if rows_bad > 0 && !repair {
        write_stdout_line(
            &mut stdout,
            format_args!(
                "audit_slot_hashes_verdict FAIL rows_bad={rows_bad}: Base rows record slot hashes that do not cover their stored vectors; re-run with --repair against a stopped daemon"
            ),
        )?;
    }
    Ok(())
}

/// Measures how many derived constellations can still reach the source row
/// they were derived from (issue #1882).
///
/// A constellation's `CxId` is content-addressed over the exact input bytes it
/// was measured from. Once the source row is evicted those bytes exist nowhere,
/// so the row can never be re-derived, re-encoded to a newer lens generation,
/// or audited against its own derivation. Several designs assume otherwise
/// (#1878 re-derive, #1668 lazy backfill, #1686 fingerprint regeneration), so
/// the uncoverable population is the number that decides whether any of them is
/// sound on this vault.
///
/// Read-only, and safe to point at the live vault: it builds one key set per
/// referenced source CF and tests membership, rather than doing a lookup per
/// row.
#[allow(
    clippy::too_many_lines,
    clippy::needless_pass_by_value,
    reason = "the audit owns its path and prints the whole scan and per-panel tally inline"
)]
fn audit_source_coverage(db_path: PathBuf) -> Result<(), Box<dyn Error>> {
    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    let vault = SynapseCalyxReadOnlyVault::open_existing_with_cfs(
        SynapseCalyxConfig::from_vault_dir(db_path.clone()),
        None,
    )?;
    let snapshot = vault.latest_seq();
    write_stdout_line(
        &mut stdout,
        format_args!(
            "audit_source_coverage db_path={} mode=read_only vault_id={} snapshot={snapshot}",
            db_path.display(),
            vault.vault_id(),
        ),
    )?;

    // (panel, source_cf) -> (covered, uncoverable)
    let mut tally: BTreeMap<(String, String), (u64, u64)> = BTreeMap::new();
    // source_cf -> set of present source keys, loaded once on first reference.
    let mut source_keys: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut rows_scanned = 0_u64;
    let mut rows_unattributed = 0_u64;
    let mut rows_covered = 0_u64;
    let mut rows_uncoverable = 0_u64;
    let mut rows_non_synapse_source = 0_u64;
    let mut non_synapse_source_cfs: BTreeSet<String> = BTreeSet::new();

    for (key, value) in vault.scan_cf_at(snapshot, ColumnFamily::Base)? {
        rows_scanned = rows_scanned.saturating_add(1);
        let constellation = decode_constellation_base(&value).map_err(|error| {
            format!(
                "SOURCE_AUDIT_BASE_DECODE_FAILED key_hex={} value_len_bytes={} value_sha256={}: {error}",
                hex_encode(&key),
                value.len(),
                sha256_hex(&value)
            )
        })?;
        let panel = constellation
            .metadata
            .get(META_PANEL_NAME)
            .cloned()
            .unwrap_or_else(|| format!("panel_version={}", constellation.panel_version));
        let (Some(source_cf), Some(source_key_hex)) = (
            constellation.metadata.get(META_SOURCE_CF),
            constellation.metadata.get(META_SOURCE_KEY_HEX),
        ) else {
            // A derived row that never recorded where it came from cannot be
            // re-derived either, and is reported as its own class rather than
            // folded into "covered".
            rows_unattributed = rows_unattributed.saturating_add(1);
            write_stdout_line(
                &mut stdout,
                format_args!(
                    "source_unattributed cx_id={} panel={panel} has_source_cf={} has_source_key={}",
                    constellation.cx_id,
                    constellation.metadata.contains_key(META_SOURCE_CF),
                    constellation.metadata.contains_key(META_SOURCE_KEY_HEX),
                ),
            )?;
            continue;
        };

        if !source_keys.contains_key(source_cf) && !non_synapse_source_cfs.contains(source_cf) {
            // `include_expired = true`: a row still physically present but past
            // its TTL is not a row a re-derive can rely on, but counting it as
            // already gone would overstate the loss. It is counted as present
            // here and the TTL is reported separately by the GC surface.
            // A source CF that is not part of the Synapse storage schema is a
            // distinct fact from an evicted row, and is reported as such rather
            // than counted as loss. `syn-recurrence-subject-v1` is the live
            // example: it names CALYX_RECURRENCE_SUBJECT, a Calyx-internal
            // derivation input with no Synapse CF behind it, so its rows have
            // no Synapse source row to outlive in the first place.
            match scan_cf_read_only_with_expired(
                Path::new(&db_path),
                synapse_core::SCHEMA_VERSION,
                StorageBackendKind::Calyx,
                source_cf,
                true,
            ) {
                Ok(rows) => {
                    let keys = rows
                        .iter()
                        .map(|(key, _)| hex_encode(key))
                        .collect::<BTreeSet<_>>();
                    write_stdout_line(
                        &mut stdout,
                        format_args!(
                            "source_cf_loaded source_cf={source_cf} rows_present={}",
                            keys.len()
                        ),
                    )?;
                    source_keys.insert(source_cf.clone(), keys);
                }
                Err(error) => {
                    write_stdout_line(
                        &mut stdout,
                        format_args!("source_cf_not_synapse source_cf={source_cf} detail={error}"),
                    )?;
                    non_synapse_source_cfs.insert(source_cf.clone());
                }
            }
        }
        if non_synapse_source_cfs.contains(source_cf) {
            rows_non_synapse_source = rows_non_synapse_source.saturating_add(1);
            continue;
        }
        let present = source_keys
            .get(source_cf)
            .is_some_and(|keys| keys.contains(source_key_hex));
        let entry = tally.entry((panel.clone(), source_cf.clone())).or_default();
        if present {
            entry.0 = entry.0.saturating_add(1);
            rows_covered = rows_covered.saturating_add(1);
        } else {
            entry.1 = entry.1.saturating_add(1);
            rows_uncoverable = rows_uncoverable.saturating_add(1);
        }
    }

    for ((panel, source_cf), (covered, uncoverable)) in &tally {
        write_stdout_line(
            &mut stdout,
            format_args!(
                "source_coverage_panel panel={panel} source_cf={source_cf} derived_rows={} source_present={covered} source_evicted={uncoverable}",
                covered.saturating_add(*uncoverable)
            ),
        )?;
    }
    write_stdout_line(
        &mut stdout,
        format_args!(
            "audit_source_coverage_summary rows_scanned={rows_scanned} rows_attributed={} source_present={rows_covered} source_evicted={rows_uncoverable} rows_unattributed={rows_unattributed} rows_non_synapse_source={rows_non_synapse_source} non_synapse_source_cfs={}",
            rows_covered.saturating_add(rows_uncoverable),
            if non_synapse_source_cfs.is_empty() {
                "<none>".to_owned()
            } else {
                non_synapse_source_cfs
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(",")
            }
        ),
    )?;
    if rows_uncoverable > 0 {
        write_stdout_line(
            &mut stdout,
            format_args!(
                "audit_source_coverage_verdict UNCOVERABLE rows={rows_uncoverable}: these derived constellations can never be re-derived, re-encoded to a newer lens generation, or audited against their own derivation, because the input bytes their CxId addresses no longer exist anywhere (#1882)"
            ),
        )?;
    }
    Ok(())
}
