//! Diagnostic probe: which cells does a query text land in, and does the live
//! BM25 sidecar actually carry postings for them?
//!
//! `by_text` returning zero hits has two very different causes that look
//! identical from the outside: the query measured into cells the index has no
//! postings for (a real "nothing matches"), or the query measured into *no
//! cells at all* (a broken measurement, which `sparse::search` short-circuits
//! to an empty result before scoring anything).
//!
//! This separates them from the bytes: it measures the query with the
//! production `syn_sparse_text_tf` encoder at the live lane's dimension, prints
//! the cells, and reports how many rows the persisted sidecar has posted in
//! each one.
//!
//! Usage:
//! `cargo run -p synapse-calyx --example text_lane_probe_fsv -- <sidecar.json> <dim> <text>...`

use std::collections::BTreeMap;
use std::error::Error;
use std::fs;

use calyx_core::{Input, Lens as _, Modality, SlotVector};
use calyx_registry::AlgorithmicLens;

fn main() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args().skip(1);
    let sidecar_path = args
        .next()
        .ok_or("usage: text_lane_probe_fsv <sidecar.json> <dim> <text>...")?;
    let dim: u32 = args.next().ok_or("missing <dim>")?.parse()?;
    let texts = args.collect::<Vec<_>>();
    if texts.is_empty() {
        return Err("supply at least one query text".into());
    }

    let sidecar: serde_json::Value = serde_json::from_slice(&fs::read(&sidecar_path)?)?;
    println!(
        "sidecar {} scoring={} dim={} rows={}",
        sidecar_path,
        sidecar["scoring"],
        sidecar["dim"],
        sidecar["rows"].as_array().map_or(0, Vec::len)
    );
    let postings = sidecar["postings"]
        .as_object()
        .ok_or("sidecar has no postings object")?;
    println!("distinct posted cells = {}", postings.len());

    // The lens name does not enter the hash, only the frozen kind + dim do, so
    // this measures into exactly the space the live lane was built in.
    let lens = AlgorithmicLens::syn_sparse_text_tf("probe.v1", Modality::Structured, dim);

    for text in &texts {
        let vector = lens.measure(&Input::new(Modality::Structured, text.as_bytes().to_vec()))?;
        let SlotVector::Sparse { entries, .. } = vector else {
            return Err("expected a sparse vector".into());
        };
        let cells: BTreeMap<u32, f32> = entries.iter().map(|e| (e.idx, e.val)).collect();
        println!("\nquery {text:?}");
        println!("  measured_cells = {}", cells.len());
        if cells.is_empty() {
            println!("  >>> EMPTY MEASUREMENT: sparse::search short-circuits to zero hits");
            continue;
        }
        let mut matched = 0_usize;
        for (cell, weight) in &cells {
            let posted = postings
                .get(&cell.to_string())
                .and_then(serde_json::Value::as_array)
                .map_or(0, Vec::len);
            if posted > 0 {
                matched += 1;
            }
            println!("  cell {cell:<6} qtf={weight:<5} rows_posted_here={posted}");
        }
        println!("  cells_with_postings = {matched}/{}", cells.len());
    }
    Ok(())
}
