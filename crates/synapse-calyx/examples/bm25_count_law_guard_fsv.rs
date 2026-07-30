//! Manual FSV instrument for the #1902 build/read-time count-law guard.
//!
//! `bm25_length_saturation_fsv` proves *why* a fractional BM25 lane is broken.
//! This one proves the guard that refuses to serve one, and it does so against
//! **real production bytes**: it copies the live vault's persisted search
//! generation into a scratch directory and perturbs exactly one number.
//!
//! ## The smallest change that proves it
//!
//! The live generation's slot 103 is the only `sparse_bm25` lane on this host,
//! measured by `syn_sparse_text_tf`, so every stored weight is a whole count.
//! The instrument:
//!
//! 1. copies the generation and searches it **unmodified** — establishing that
//!    the copy is intact and the guard does not fire on a genuine count lane;
//! 2. rewrites **one weight** in **one row** from its integer count to that
//!    count plus 0.5, recomputes the sidecar's sha256, patches the manifest so
//!    the integrity check still passes, and searches again.
//!
//! Step 2's perturbation is the minimal difference between a count lane and a
//! normalized one: one weight that is not a whole number. If the guard is real,
//! the second search must fail closed naming that weight; if it is decorative,
//! the second search returns hits and the inert-`b` lane serves recall.
//!
//! Nothing is written to the live vault: the scratch copy is the only thing
//! mutated, and the live path is opened read-only for the copy.
//!
//! Usage:
//! `cargo run -p synapse-calyx --example bm25_count_law_guard_fsv -- <live-vault-dir> <scratch-dir>`

use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};

use calyx_core::{SlotId, SlotVector, SparseEntry};
use calyx_search::PersistedSearchIndexes;
use sha2::{Digest as _, Sha256};

const PANEL_VERSION: u32 = 1_900_001;
const BM25_SLOT: u16 = 103;

fn sha256_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .fold(String::new(), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
}

fn copy_generation(live: &Path, scratch: &Path) -> Result<PathBuf, Box<dyn Error>> {
    let rel = PathBuf::from("idx")
        .join("search")
        .join(format!("panel_{PANEL_VERSION:010}"));
    let src = live.join(&rel);
    let dst = scratch.join(&rel);
    fs::create_dir_all(&dst)?;
    for entry in fs::read_dir(&src)? {
        let entry = entry?;
        if entry.file_type()?.is_file() {
            fs::copy(entry.path(), dst.join(entry.file_name()))?;
        }
    }
    Ok(dst)
}

/// Reads the sidecar path the manifest declares for the BM25 slot.
fn bm25_sidecar_rel(manifest: &serde_json::Value) -> Result<String, Box<dyn Error>> {
    manifest["slots"]
        .as_array()
        .ok_or("manifest has no slots array")?
        .iter()
        .find(|slot| slot["slot"].as_u64() == Some(u64::from(BM25_SLOT)))
        .and_then(|slot| slot["index_rel"].as_str())
        .map(ToOwned::to_owned)
        .ok_or_else(|| format!("manifest declares no slot {BM25_SLOT}").into())
}

/// A one-cell query on the cell the chosen row actually carries, so a working
/// lane must return at least that row.
fn query_for(cell: u32, dim: u32) -> SlotVector {
    SlotVector::Sparse {
        dim,
        entries: vec![SparseEntry {
            idx: cell,
            val: 1.0,
        }],
    }
}

fn describe(
    result: &Result<Vec<calyx_sextant::index::IndexSearchHit>, calyx_search::SearchError>,
) -> String {
    match result {
        Ok(hits) => format!("Ok({} hits)", hits.len()),
        Err(error) => format!("Err({error})"),
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args().skip(1);
    let live = PathBuf::from(
        args.next()
            .ok_or("usage: bm25_count_law_guard_fsv <live-vault-dir> <scratch-dir>")?,
    );
    let scratch = PathBuf::from(args.next().ok_or("missing <scratch-dir>")?);
    fs::create_dir_all(&scratch)?;

    let generation = copy_generation(&live, &scratch)?;
    println!(
        "copied live generation {PANEL_VERSION} -> {}",
        generation.display()
    );

    let manifest_path = generation.join("manifest.json");
    let mut manifest: serde_json::Value = serde_json::from_slice(&fs::read(&manifest_path)?)?;
    let sidecar_rel = bm25_sidecar_rel(&manifest)?;
    let sidecar_path = scratch.join(sidecar_rel.replace('/', std::path::MAIN_SEPARATOR_STR));

    let mut sidecar: serde_json::Value = serde_json::from_slice(&fs::read(&sidecar_path)?)?;
    println!(
        "sidecar scoring={} dim={} rows={}",
        sidecar["scoring"],
        sidecar["dim"],
        sidecar["rows"].as_array().map_or(0, Vec::len)
    );

    // Pick the first row that actually carries a term; that row's first cell is
    // the query, so a working lane must find it.
    let (row_index, cell, original_weight) = {
        let rows = sidecar["rows"].as_array().ok_or("sidecar has no rows")?;
        let (index, row) = rows
            .iter()
            .enumerate()
            .find(|(_, row)| !row["entries"].as_array().is_none_or(Vec::is_empty))
            .ok_or("no row carries a term")?;
        let entry = &row["entries"][0];
        (
            index,
            u32::try_from(entry["idx"].as_u64().ok_or("entry idx")?)?,
            entry["val"].as_f64().ok_or("entry val")?,
        )
    };
    let dim = u32::try_from(sidecar["dim"].as_u64().ok_or("dim")?)?;
    let cx_id = sidecar["rows"][row_index]["cx_id"].clone();
    println!(
        "chosen row #{row_index} cx_id={cx_id} cell={cell} weight={original_weight} (a whole count)"
    );

    // --- CASE 1: the unmodified production copy ---------------------------
    let case1 = PersistedSearchIndexes::open(&scratch, PANEL_VERSION)?.search(
        SlotId::new(BM25_SLOT),
        &query_for(cell, dim),
        5,
    );
    println!("CASE1 pristine_production_bytes -> {}", describe(&case1));

    // --- CASE 2: exactly one weight made fractional ------------------------
    let perturbed = original_weight + 0.5;
    sidecar["rows"][row_index]["entries"][0]["val"] = serde_json::json!(perturbed);
    // doc_len is validated against the weight sum, so it must move with it or
    // the run would be stopped by a different check and prove nothing.
    let doc_len = sidecar["rows"][row_index]["doc_len"]
        .as_f64()
        .ok_or("doc_len")?;
    sidecar["rows"][row_index]["doc_len"] = serde_json::json!(doc_len + 0.5);
    if let Some(lengths) = sidecar["doc_lengths"].as_object_mut() {
        let key = cx_id.as_str().ok_or("cx_id string")?.to_owned();
        lengths.insert(key, serde_json::json!(doc_len + 0.5));
    }
    // Postings carry the same tf and are cross-checked against the rows.
    if let Some(postings) = sidecar["postings"]
        .as_object_mut()
        .and_then(|map| map.get_mut(&cell.to_string()))
        .and_then(serde_json::Value::as_array_mut)
    {
        for posting in postings.iter_mut() {
            if posting["cx_id"] == cx_id {
                posting["tf"] = serde_json::json!(perturbed);
            }
        }
    }

    let bytes = serde_json::to_vec(&sidecar)?;
    fs::write(&sidecar_path, &bytes)?;
    let digest = sha256_hex(&bytes);
    for slot in manifest["slots"]
        .as_array_mut()
        .ok_or("manifest slots")?
        .iter_mut()
    {
        if slot["slot"].as_u64() == Some(u64::from(BM25_SLOT)) {
            slot["sha256"] = serde_json::json!(digest);
        }
    }
    fs::write(&manifest_path, serde_json::to_vec(&manifest)?)?;
    println!(
        "perturbed row #{row_index} weight {original_weight} -> {perturbed}, resealed sha256={digest}"
    );

    let case2 = PersistedSearchIndexes::open(&scratch, PANEL_VERSION)?.search(
        SlotId::new(BM25_SLOT),
        &query_for(cell, dim),
        5,
    );
    println!("CASE2 one_fractional_weight -> {}", describe(&case2));

    Ok(())
}
