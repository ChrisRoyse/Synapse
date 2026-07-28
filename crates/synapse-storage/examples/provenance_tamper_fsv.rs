//! Manual FSV instrument for provenance tamper REJECTION (issue #1874).
//!
//! `audit operation=verify_chain` has only ever been observed reporting
//! `intact`. A tamper-evidence mechanism whose detection path is unexercised is
//! advertising a property it has not been shown to have, and `verdict=intact`
//! looks identical whether it is computing hard or trivially returning success.
//!
//! Proving rejection requires mutating a sealed commitment / Ledger row and
//! re-verifying. Doing that to the live production vault is not an acceptable
//! trade, so this driver operates on a **disposable vault at a path you name**.
//! Everything it touches is real: a genuine Aster vault opened by the real
//! `SynapseCalyxVault` code, real WAL/MVCC commits, the real checkpoint sealer,
//! and the real `verify_ledger_chain` verifier. Nothing is mocked or stubbed.
//!
//! The public write path refuses to touch either provenance CF at all —
//! `CALYX_ASTER_RAW_COMMITMENT_WRITE_FORBIDDEN` and
//! `CALYX_ASTER_LEDGER_RAW_WRITE_FORBIDDEN` — so the API-level tamper vector is
//! already closed, and `mutate-commitment` / `mutate-ledger` exist to
//! demonstrate exactly that. Placing tampered provenance in front of the
//! verifier therefore means writing it *underneath* the API, which is also what
//! real corruption and a real attacker do. A naive byte flip would only trip
//! the storage layer's own record/body CRC and prove nothing about provenance,
//! so `sst-patch-value` / `sst-drop-value` recompute both CRC layers: the
//! storage layer accepts the file as intact, and a structurally valid row
//! carrying a different commitment is left for the Merkle seal to catch. That
//! is the failure mode a recompute-and-compare verifier is actually fooled by.
//!
//! Usage (run each step as its own process so every readback is an independent
//! open of the durable bytes):
//!
//! ```text
//! cargo run -p synapse-storage --example provenance_tamper_fsv -- seed <vault_dir> <batches>
//! cargo run -p synapse-storage --example provenance_tamper_fsv -- verify <vault_dir>
//! cargo run -p synapse-storage --example provenance_tamper_fsv -- list-commitments <vault_dir>
//! cargo run -p synapse-storage --example provenance_tamper_fsv -- mutate-commitment <vault_dir> <seq>
//! cargo run -p synapse-storage --example provenance_tamper_fsv -- delete-commitment <vault_dir> <seq>
//! cargo run -p synapse-storage --example provenance_tamper_fsv -- mutate-ledger <vault_dir> <seq>
//! cargo run -p synapse-storage --example provenance_tamper_fsv -- lineage <vault_dir>
//! cargo run -p synapse-storage --example provenance_tamper_fsv -- sst-patch-value <cf_dir> <old_hex> <new_hex>
//! cargo run -p synapse-storage --example provenance_tamper_fsv -- sst-drop-value <cf_dir> <value_hex>
//! ```
//!
//! `sst-patch-value` / `sst-drop-value` operate on the physical SST bytes with
//! the vault closed. They exist because the public write path refuses to touch
//! the commitment CF at all (`CALYX_ASTER_RAW_COMMITMENT_WRITE_FORBIDDEN`), so
//! the only way to place tampered provenance in front of the verifier is to
//! write it underneath the API — which is also what real corruption or a real
//! attacker does. Both rewrite the record CRC and the SST body CRC so the
//! storage layer accepts the file as intact; anything the verifier then reports
//! is the provenance layer's own detection, not a CRC failure.

use std::error::Error;
use std::path::PathBuf;

use calyx_aster::cf::{ColumnFamily, ledger_key};
use calyx_aster::mvcc::tombstone_value;
use synapse_calyx::{SynapseCalyxCfWrite, SynapseCalyxConfig, SynapseCalyxVault};

const USAGE: &str = "usage: provenance_tamper_fsv \
                     <seed|verify|list-commitments|mutate-commitment|delete-commitment|\
                     mutate-ledger|lineage|sst-patch-value|sst-drop-value> <dir> [args]";

/// SST layout (`calyx-aster::sst`): `CXS1` | version u32 | entries u32 |
/// `index_offset` u64 | `bloom_offset` u64 | `body_crc` u32, then records, then
/// the index, then the bloom filter.
const SST_MAGIC: &[u8; 4] = b"CXS1";
const SST_HEADER_LEN: usize = 32;
const SST_RECORD_HEADER_LEN: usize = 12;

/// `CYXRAW01` + seq(8) + `row_count`(8) + `batch_hash`(32).
const RAW_COMMITMENT_VALUE_BYTES: usize = 8 + 8 + 8 + 32;
const RAW_COMMITMENT_MAGIC: &[u8; 8] = b"CYXRAW01";
const RAW_COMMITMENT_HASH_OFFSET: usize = 24;

fn main() -> Result<(), Box<dyn Error>> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    let (command, vault_dir) = match (args.first(), args.get(1)) {
        (Some(command), Some(dir)) => (command.as_str(), PathBuf::from(dir)),
        _ => return Err(USAGE.into()),
    };
    let config = SynapseCalyxConfig::from_vault_dir(vault_dir.clone());

    match command {
        "seed" => {
            let batches = args
                .get(2)
                .ok_or("seed requires a batch count")?
                .parse::<u32>()?;
            seed(config, batches)
        }
        "verify" => verify(config),
        "list-commitments" => list_commitments(config),
        "mutate-commitment" => mutate_commitment(config, parse_seq(args.get(2))?),
        "delete-commitment" => delete_commitment(config, parse_seq(args.get(2))?),
        "mutate-ledger" => mutate_ledger(config, parse_seq(args.get(2))?),
        "lineage" => lineage(config),
        // These take a CF directory, not a vault, and never open the vault.
        "sst-patch-value" => sst_patch_value(
            &vault_dir,
            &parse_hex(args.get(2).ok_or("missing old value hex")?)?,
            &parse_hex(args.get(3).ok_or("missing new value hex")?)?,
        ),
        "sst-drop-value" => sst_drop_value(
            &vault_dir,
            &parse_hex(args.get(2).ok_or("missing value hex")?)?,
        ),
        _ => Err(USAGE.into()),
    }
}

fn parse_hex(text: &str) -> Result<Vec<u8>, Box<dyn Error>> {
    if !text.len().is_multiple_of(2) {
        return Err(format!("hex string has odd length {}", text.len()).into());
    }
    (0..text.len())
        .step_by(2)
        .map(|index| {
            u8::from_str_radix(&text[index..index + 2], 16)
                .map_err(|error| format!("bad hex at {index}: {error}").into())
        })
        .collect()
}

struct SstFile {
    version: u32,
    records: Vec<(Vec<u8>, Vec<u8>)>,
    bloom: Vec<u8>,
}

/// Parses an SST far enough to rewrite it. Deliberately re-derives every CRC it
/// checks, so a file this refuses to parse was already unreadable to Aster.
fn parse_sst(bytes: &[u8]) -> Result<SstFile, Box<dyn Error>> {
    if bytes.len() < SST_HEADER_LEN || &bytes[0..4] != SST_MAGIC {
        return Err("not an SST (magic mismatch)".into());
    }
    let version = u32::from_le_bytes(bytes[4..8].try_into()?);
    let entries = u32::from_le_bytes(bytes[8..12].try_into()?) as usize;
    let index_offset = usize::try_from(u64::from_le_bytes(bytes[12..20].try_into()?))?;
    let bloom_offset = usize::try_from(u64::from_le_bytes(bytes[20..28].try_into()?))?;
    if index_offset > bytes.len() || bloom_offset < index_offset || bloom_offset > bytes.len() {
        return Err("SST header offsets out of bounds".into());
    }
    let mut records = Vec::with_capacity(entries);
    let mut offset = SST_HEADER_LEN;
    while offset < index_offset {
        let header = bytes
            .get(offset..offset + SST_RECORD_HEADER_LEN)
            .ok_or("SST record header out of bounds")?;
        let key_len = u32::from_le_bytes(header[0..4].try_into()?) as usize;
        let value_len = u32::from_le_bytes(header[4..8].try_into()?) as usize;
        let expected_crc = u32::from_le_bytes(header[8..12].try_into()?);
        let key_start = offset + SST_RECORD_HEADER_LEN;
        let value_start = key_start + key_len;
        let value_end = value_start + value_len;
        let key = bytes.get(key_start..value_start).ok_or("key OOB")?.to_vec();
        let value = bytes
            .get(value_start..value_end)
            .ok_or("value OOB")?
            .to_vec();
        if record_crc(&key, &value) != expected_crc {
            return Err(format!("SST record crc mismatch at offset {offset}").into());
        }
        records.push((key, value));
        offset = value_end;
    }
    if records.len() != entries {
        return Err(format!("SST entry count {} != header {entries}", records.len()).into());
    }
    Ok(SstFile {
        version,
        records,
        bloom: bytes[bloom_offset..].to_vec(),
    })
}

/// Re-encodes an SST from records, reusing the original bloom filter verbatim.
///
/// A bloom that still carries a dropped key is a false positive, which the
/// format already tolerates by design — the index lookup is authoritative — so
/// reusing it keeps the rewrite honest about what it changed.
fn encode_sst(file: &SstFile) -> Vec<u8> {
    let mut bytes = vec![0_u8; SST_HEADER_LEN];
    let mut index: Vec<(Vec<u8>, u64)> = Vec::with_capacity(file.records.len());
    for (key, value) in &file.records {
        let offset = bytes.len() as u64;
        bytes.extend_from_slice(&u32::try_from(key.len()).unwrap_or(u32::MAX).to_le_bytes());
        bytes.extend_from_slice(&u32::try_from(value.len()).unwrap_or(u32::MAX).to_le_bytes());
        bytes.extend_from_slice(&record_crc(key, value).to_le_bytes());
        bytes.extend_from_slice(key);
        bytes.extend_from_slice(value);
        index.push((key.clone(), offset));
    }
    let index_offset = bytes.len() as u64;
    for (key, offset) in &index {
        bytes.extend_from_slice(&u32::try_from(key.len()).unwrap_or(u32::MAX).to_le_bytes());
        bytes.extend_from_slice(&offset.to_le_bytes());
        bytes.extend_from_slice(key);
    }
    let bloom_offset = bytes.len() as u64;
    bytes.extend_from_slice(&file.bloom);
    let body_crc = section_crc(&bytes[SST_HEADER_LEN..]);
    bytes[0..4].copy_from_slice(SST_MAGIC);
    bytes[4..8].copy_from_slice(&file.version.to_le_bytes());
    bytes[8..12].copy_from_slice(
        &u32::try_from(file.records.len())
            .unwrap_or(u32::MAX)
            .to_le_bytes(),
    );
    bytes[12..20].copy_from_slice(&index_offset.to_le_bytes());
    bytes[20..28].copy_from_slice(&bloom_offset.to_le_bytes());
    bytes[28..32].copy_from_slice(&body_crc.to_le_bytes());
    bytes
}

fn record_crc(key: &[u8], value: &[u8]) -> u32 {
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(key);
    hasher.update(value);
    hasher.finalize()
}

fn section_crc(bytes: &[u8]) -> u32 {
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(bytes);
    hasher.finalize()
}

/// Replaces every record whose value equals `old` with `new` (same length), in
/// every SST under `cf_dir`, fixing both CRC layers.
fn sst_patch_value(cf_dir: &PathBuf, old: &[u8], new: &[u8]) -> Result<(), Box<dyn Error>> {
    if old.len() != new.len() {
        return Err(format!(
            "sst-patch-value requires equal lengths: old={} new={}",
            old.len(),
            new.len()
        )
        .into());
    }
    let mut patched_total = 0_usize;
    for entry in std::fs::read_dir(cf_dir)? {
        let path = entry?.path();
        if path.extension().and_then(|value| value.to_str()) != Some("sst") {
            continue;
        }
        let bytes = std::fs::read(&path)?;
        let mut file = match parse_sst(&bytes) {
            Ok(file) => file,
            Err(error) => {
                println!("SST_SKIP path={} reason={error}", path.display());
                continue;
            }
        };
        let mut patched = 0_usize;
        for (key, value) in &mut file.records {
            if value.as_slice() == old {
                println!(
                    "SST_PATCH path={} key={} old={} new={}",
                    path.display(),
                    hex(key),
                    hex(old),
                    hex(new)
                );
                *value = new.to_vec();
                patched += 1;
            }
        }
        if patched > 0 {
            std::fs::write(&path, encode_sst(&file))?;
            let verified = parse_sst(&std::fs::read(&path)?)?;
            println!(
                "SST_PATCH_WRITTEN path={} records={} crc_reparse_ok=true",
                path.display(),
                verified.records.len()
            );
            patched_total += patched;
        }
    }
    println!("SST_PATCH_TOTAL patched_records={patched_total}");
    if patched_total == 0 {
        return Err("no SST record matched the supplied value".into());
    }
    Ok(())
}

/// Removes every record whose value equals `value_to_drop`, rebuilding the
/// record region and index.
fn sst_drop_value(cf_dir: &PathBuf, value_to_drop: &[u8]) -> Result<(), Box<dyn Error>> {
    let mut dropped_total = 0_usize;
    for entry in std::fs::read_dir(cf_dir)? {
        let path = entry?.path();
        if path.extension().and_then(|value| value.to_str()) != Some("sst") {
            continue;
        }
        let bytes = std::fs::read(&path)?;
        let mut file = match parse_sst(&bytes) {
            Ok(file) => file,
            Err(error) => {
                println!("SST_SKIP path={} reason={error}", path.display());
                continue;
            }
        };
        let before = file.records.len();
        for (key, value) in &file.records {
            if value.as_slice() == value_to_drop {
                println!(
                    "SST_DROP path={} key={} value={}",
                    path.display(),
                    hex(key),
                    hex(value)
                );
            }
        }
        file.records
            .retain(|(_key, value)| value.as_slice() != value_to_drop);
        let dropped = before - file.records.len();
        if dropped > 0 {
            std::fs::write(&path, encode_sst(&file))?;
            let verified = parse_sst(&std::fs::read(&path)?)?;
            println!(
                "SST_DROP_WRITTEN path={} records_before={before} records_after={} crc_reparse_ok=true",
                path.display(),
                verified.records.len()
            );
            dropped_total += dropped;
        }
    }
    println!("SST_DROP_TOTAL dropped_records={dropped_total}");
    if dropped_total == 0 {
        return Err("no SST record matched the supplied value".into());
    }
    Ok(())
}

fn parse_seq(arg: Option<&String>) -> Result<u64, Box<dyn Error>> {
    Ok(arg
        .ok_or("this command requires a sequence number")?
        .parse()?)
}

/// Writes `batches` distinct raw CF batches and forces a checkpoint so the
/// commitment cohort seals into the Ledger. Each batch is one real WAL/MVCC
/// commit, so each produces exactly one raw-commitment row.
fn seed(config: SynapseCalyxConfig, batches: u32) -> Result<(), Box<dyn Error>> {
    let vault = SynapseCalyxVault::open(config)?;
    println!("SEED_BEFORE latest_seq={}", vault.latest_seq());
    for batch in 0..batches {
        // A payload whose exact bytes are known, so a later readback proves the
        // row is the one this driver wrote and not an artefact.
        let key = format!("fsv-1874/row-{batch:04}").into_bytes();
        let value = format!("fsv-1874-payload-batch-{batch:04}").into_bytes();
        let seq = vault.write_cf_batch(vec![SynapseCalyxCfWrite::new(
            ColumnFamily::Kv,
            key.clone(),
            value.clone(),
        )])?;
        println!(
            "SEED_BATCH batch={batch} committed_seq={seq} key={} value_len={}",
            String::from_utf8_lossy(&key),
            value.len()
        );
    }
    vault.checkpoint()?;
    println!("SEED_AFTER latest_seq={}", vault.latest_seq());
    let report = vault.verify_ledger_chain(None)?;
    println!("SEED_VERIFY {}", serde_json::to_string(&report)?);
    let readback = vault.close("fsv-1874-seed")?;
    println!("SEED_CLOSED latest_seq={:?}", readback.latest_seq);
    Ok(())
}

/// Independent readback: opens the durable vault and re-walks the whole chain.
fn verify(config: SynapseCalyxConfig) -> Result<(), Box<dyn Error>> {
    let vault = SynapseCalyxVault::open(config)?;
    let report = vault.verify_ledger_chain(None)?;
    println!("VERIFY {}", serde_json::to_string(&report)?);
    vault.close("fsv-1874-verify")?;
    Ok(())
}

/// Prints every physical raw-commitment row so a tamper target can be chosen by
/// its exact sequence and its before-state recorded.
fn list_commitments(config: SynapseCalyxConfig) -> Result<(), Box<dyn Error>> {
    let vault = SynapseCalyxVault::open(config)?;
    let seq = vault.latest_seq();
    for (key, value) in vault.scan_cf_at(seq, ColumnFamily::RawCommitment)? {
        println!(
            "COMMITMENT key_seq={} value_len={} magic_ok={} row_count={} batch_hash={}",
            decode_be_u64(&key),
            value.len(),
            value.starts_with(RAW_COMMITMENT_MAGIC),
            decode_be_u64(&value[16..24]),
            hex(&value[RAW_COMMITMENT_HASH_OFFSET..]),
        );
    }
    vault.close("fsv-1874-list")?;
    Ok(())
}

/// Case 2/4 of #1874: replace a **sealed** commitment's Merkle leaf material
/// with a structurally valid row carrying a different batch hash.
///
/// The row still decodes: right magic, right sequence, right row count. Only
/// the committed batch hash differs, which is precisely what the cohort's
/// Merkle root binds. A verifier that merely re-read and re-parsed the row
/// would call this intact.
fn mutate_commitment(config: SynapseCalyxConfig, seq: u64) -> Result<(), Box<dyn Error>> {
    let vault = SynapseCalyxVault::open(config)?;
    let key = seq.to_be_bytes().to_vec();
    let before = vault
        .read_cf_latest(ColumnFamily::RawCommitment, &key)?
        .ok_or_else(|| format!("no raw-commitment row at seq={seq}"))?;
    if before.len() != RAW_COMMITMENT_VALUE_BYTES {
        return Err(format!(
            "unexpected raw-commitment row length {} at seq={seq}; expected {RAW_COMMITMENT_VALUE_BYTES}",
            before.len()
        )
        .into());
    }
    println!("MUTATE_COMMITMENT_BEFORE seq={seq} value={}", hex(&before));
    let mut after = before.clone();
    // Flip the low bit of the final batch-hash byte: the smallest change that
    // still leaves every structural field byte-identical.
    let last = after.len() - 1;
    after[last] ^= 0x01;
    vault.write_cf_batch(vec![SynapseCalyxCfWrite::new(
        ColumnFamily::RawCommitment,
        key.clone(),
        after.clone(),
    )])?;
    let readback = vault
        .read_cf_latest(ColumnFamily::RawCommitment, &key)?
        .ok_or("raw-commitment row vanished after write")?;
    println!("MUTATE_COMMITMENT_AFTER seq={seq} value={}", hex(&readback));
    println!(
        "MUTATE_COMMITMENT_APPLIED changed={} structurally_valid={}",
        readback != before,
        readback.starts_with(RAW_COMMITMENT_MAGIC) && readback.len() == before.len()
    );
    vault.close("fsv-1874-mutate-commitment")?;
    Ok(())
}

/// Case 4 of #1874: remove a row from inside a sealed cohort. A missing row
/// must not silently shrink the cohort into a valid smaller root.
fn delete_commitment(config: SynapseCalyxConfig, seq: u64) -> Result<(), Box<dyn Error>> {
    let vault = SynapseCalyxVault::open(config)?;
    let key = seq.to_be_bytes().to_vec();
    let before = vault.read_cf_latest(ColumnFamily::RawCommitment, &key)?;
    println!(
        "DELETE_COMMITMENT_BEFORE seq={seq} present={} value={}",
        before.is_some(),
        before.as_deref().map_or_else(String::new, hex)
    );
    vault.write_cf_batch(vec![SynapseCalyxCfWrite::new(
        ColumnFamily::RawCommitment,
        key.clone(),
        tombstone_value(),
    )])?;
    let after = vault.read_cf_latest(ColumnFamily::RawCommitment, &key)?;
    println!(
        "DELETE_COMMITMENT_AFTER seq={seq} present={}",
        after.is_some()
    );
    vault.close("fsv-1874-delete-commitment")?;
    Ok(())
}

/// Case 3 of #1874: mutate a Ledger entry inside the verified range.
///
/// The Ledger CF carries an append-only invariant, so this may be refused
/// outright — a refusal is itself a real, reportable property and is printed as
/// such rather than swallowed.
fn mutate_ledger(config: SynapseCalyxConfig, seq: u64) -> Result<(), Box<dyn Error>> {
    let vault = SynapseCalyxVault::open(config)?;
    let key = ledger_key(seq);
    let before = vault
        .read_cf_latest(ColumnFamily::Ledger, &key)?
        .ok_or_else(|| format!("no Ledger row at seq={seq}"))?;
    println!(
        "MUTATE_LEDGER_BEFORE seq={seq} value_len={} value={}",
        before.len(),
        hex(&before)
    );
    let mut after = before.clone();
    let last = after.len() - 1;
    after[last] ^= 0x01;
    match vault.write_cf_batch(vec![SynapseCalyxCfWrite::new(
        ColumnFamily::Ledger,
        key.clone(),
        after,
    )]) {
        Ok(committed) => {
            let readback = vault
                .read_cf_latest(ColumnFamily::Ledger, &key)?
                .ok_or("Ledger row vanished after write")?;
            println!(
                "MUTATE_LEDGER_APPLIED seq={seq} committed_seq={committed} changed={}",
                readback != before
            );
        }
        Err(error) => {
            println!("MUTATE_LEDGER_REFUSED seq={seq} error={error}");
        }
    }
    vault.close("fsv-1874-mutate-ledger")?;
    Ok(())
}

/// Prints the vault lineage journal state this open observed (issue #1875).
fn lineage(config: SynapseCalyxConfig) -> Result<(), Box<dyn Error>> {
    let vault = SynapseCalyxVault::open(config)?;
    println!("LINEAGE {}", serde_json::to_string(vault.lineage())?);
    println!(
        "LINEAGE_VAULT vault_id={} latest_seq={}",
        vault.vault_id(),
        vault.latest_seq()
    );
    vault.close("fsv-1874-lineage")?;
    Ok(())
}

fn decode_be_u64(bytes: &[u8]) -> u64 {
    let mut buf = [0_u8; 8];
    let len = bytes.len().min(8);
    buf[8 - len..].copy_from_slice(&bytes[..len]);
    u64::from_be_bytes(buf)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut out, byte| {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
        out
    })
}
