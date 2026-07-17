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

use std::{
    collections::BTreeMap,
    error::Error,
    fmt,
    io::{self, Write},
    path::{Path, PathBuf},
    str::FromStr as _,
};

use calyx_aster::{
    cf::{ColumnFamily, anchor_prefix_range, base_key, scalar_id_for_key, slot_key},
    mvcc::tombstone_value,
    vault::encode::{decode_constellation_base, inspect_slot_vector},
};
use calyx_core::{CxId, SlotId};
use synapse_calyx::{
    SynapseCalyxCfWrite, SynapseCalyxConfig, SynapseCalyxReadOnlyVault, SynapseCalyxVault,
};
use synapse_storage::{
    StorageBackendKind,
    cf::CF_EPISODES,
    constellations::{
        META_PANEL_NAME, META_SOURCE_CF, META_SOURCE_KEY_HEX, SYN_EPISODE_PANEL_NAME,
        SYN_EPISODE_PANEL_VERSION, hex_encode, sha256_hex,
    },
    dump_cf_read_only_with_expired,
};

const USAGE: &str = "usage: dump_cf [--include-expired] <db_path> <cf_name> | dump_cf --native-cx [--reveal-metadata] <db_path> <cx_id> | dump_cf --native-source [--reveal-metadata] <db_path> <source_cf> <source_key_hex> | dump_cf --repair-bad-episode-slots <db_path>";
const MAX_DURABLE_SLOT_ID: u16 = 47;

fn main() -> Result<(), Box<dyn Error>> {
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
        let mut candidate_targets =
            collect_repair_targets(&vault, before_seq, candidate.cx_id, &candidate.slot_ids)?;
        for target in candidate_targets.drain(..) {
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
            .any(|slot| slot.get() > MAX_DURABLE_SLOT_ID)
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
        if slot_id.get() > MAX_DURABLE_SLOT_ID {
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
            format_args!("metadata key={} value={}", key, rendered),
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
