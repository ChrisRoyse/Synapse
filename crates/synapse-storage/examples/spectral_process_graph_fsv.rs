//! Manual Full State Verification for `syn-graphpos-process-v1`'s spectral pass
//! (#2081).
//!
//! Reconstructs the exact transition multiset `drive_agent_spawn_graph` feeds
//! `publish_graph_position_snapshot` — `agent-session:X -> agent-spawn:Y` from
//! `CF_AGENT_EVENTS` plus `process:{parent_pid} -> process:{pid}` from
//! `CF_PROCESS_HISTORY` — by scanning a vault **read-only**, then runs the same
//! `build_transition_graph` / `eigenvector_centrality` pair the publisher runs.
//! Nothing is written, no generation is allocated, and the production vault is
//! never opened for writes.
//!
//! It also reports the graph-shape facts that decide whether the power
//! iteration can converge at all: connected components, degree distribution,
//! the pair-count maximum that rescales every edge weight, and the resulting
//! weight range. Those are the inputs to the contraction ratio, so a
//! `CALYX_SPECTRAL_NOT_CONVERGED` here is explained by the same output that
//! reports it.
//!
//! Usage:
//! `cargo run -p synapse-storage --example spectral_process_graph_fsv -- <vault_dir>`
//! `cargo run -p synapse-storage --example spectral_process_graph_fsv -- --synthetic-chain`

use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    path::{Path, PathBuf},
};

use calyx_core::CxId;
use calyx_mincut::{
    StructuralParams, TransitionEdge, build_transition_graph, eigenvector_centrality,
    structural_signatures,
};
use sha2::{Digest as _, Sha256};
use synapse_core::SCHEMA_VERSION;
use synapse_storage::{StorageBackendKind, cf, scan_cf_read_only_with_expired};

/// Byte-for-byte the private `GraphPositionKind::Process::identity_tag`.
const PROCESS_IDENTITY_TAG: &[u8] = b"synapse-graphpos-process-v1";

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let include_expired = args.iter().any(|arg| arg == "--include-expired");
    let app_lane = args.iter().any(|arg| arg == "--app");
    let positional: Vec<&String> = args.iter().filter(|arg| !arg.starts_with("--")).collect();
    let usage = "usage: spectral_process_graph_fsv [--include-expired] [--app] <vault_dir> \
                 | --synthetic-chain | --synthetic-rescaled <leaves> <hot_pair_count>";

    if args.iter().any(|arg| arg == "--synthetic-chain") {
        return report("synthetic-chain", &synthetic_chain());
    }
    if args.iter().any(|arg| arg == "--synthetic-rescaled") {
        let leaves: usize = positional.first().ok_or(usage)?.parse()?;
        let hot: u64 = positional.get(1).ok_or(usage)?.parse()?;
        return report(
            &format!("synthetic-rescaled leaves={leaves} hot_pair_count={hot}"),
            &synthetic_rescaled(leaves, hot),
        );
    }
    let dir = PathBuf::from(positional.first().ok_or(usage)?.as_str());
    let (label, transitions) = if app_lane {
        (
            "app-transition",
            collect_app_transitions(&dir, include_expired)?,
        )
    } else {
        (
            "process-and-agent-spawn",
            collect_live_transitions(&dir, include_expired)?,
        )
    };
    report(label, &transitions)
}

/// A 3-node chain `a -> b -> c`, every pair count 1.
///
/// Hand-computable end to end. `build_transition_graph` rescales both counts by
/// the maximum pair count (1), so both edge weights are exactly 1.0. The
/// symmetrized adjacency is the path `P3`, whose spectrum is `{sqrt(2), 0,
/// -sqrt(2)}` with Perron vector proportional to `(1, sqrt(2), 1)`. Normalized
/// by its maximum, `ranked_scores` must therefore return
/// `b = 1`, `a = c = 1/sqrt(2) = 0.70710678`.
fn synthetic_chain() -> Vec<(String, String, u64)> {
    vec![
        ("process:1".to_owned(), "process:2".to_owned(), 1),
        ("process:2".to_owned(), "process:3".to_owned(), 1),
    ]
}

/// A depth-1 star of `leaves` leaves, each pair observed once, sharing a graph
/// with one unrelated "hot" dyad observed `hot_pair_count` times.
///
/// This is the shape the real publisher produces whenever any single transition
/// pair is observed more often than the rest — an ordinary occurrence on both
/// graph lanes. It matters because `build_transition_graph` rescales **every**
/// edge weight by the global maximum pair count, so the hot dyad drives every
/// star edge down to `1 / hot_pair_count` without changing the graph's shape at
/// all. The star's adjacency spectral radius falls with it, and a power
/// iteration shifted by a *fixed* `1` then has contraction
/// `1 / (1 + sqrt(leaves) / hot_pair_count)`, which tends to 1 as the hot count
/// grows. The centrality answer is unchanged by the rescaling; only the
/// solver's ability to reach it is. That is the property this probe measures.
fn synthetic_rescaled(leaves: usize, hot_pair_count: u64) -> Vec<(String, String, u64)> {
    let mut transitions: Vec<(String, String, u64)> = (0..leaves)
        .map(|leaf| {
            (
                "process:center".to_owned(),
                format!("process:leaf-{leaf}"),
                1,
            )
        })
        .collect();
    transitions.push((
        "process:hot-a".to_owned(),
        "process:hot-b".to_owned(),
        hot_pair_count,
    ));
    transitions
}

/// `(from, to, observed_count)` transition triples, the shape
/// `build_transition_graph` ingests.
type TransitionTriples = Vec<(String, String, u64)>;

fn collect_app_transitions(
    dir: &Path,
    include_expired: bool,
) -> Result<TransitionTriples, Box<dyn Error>> {
    let rows = scan_cf_read_only_with_expired(
        dir,
        SCHEMA_VERSION,
        StorageBackendKind::Calyx,
        cf::CF_TIMELINE,
        include_expired,
    )?;
    let mut previous = None::<String>;
    let mut counts = BTreeMap::<(String, String), u64>::new();
    for (_key, value) in &rows {
        let Ok(record) = serde_json::from_slice::<serde_json::Value>(value) else {
            continue;
        };
        if record.get("kind").and_then(serde_json::Value::as_str) != Some("focus_change") {
            continue;
        }
        let Some(app) = record
            .get("app")
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .map(str::to_owned)
        else {
            continue;
        };
        if let Some(from) = previous.replace(app.clone())
            && from != app
        {
            *counts.entry((from, app)).or_default() += 1;
        }
    }
    println!(
        "SOURCE_OF_TRUTH vault={} timeline_rows={} distinct_pairs={}",
        dir.display(),
        rows.len(),
        counts.len()
    );
    Ok(counts
        .into_iter()
        .map(|((src, dst), count)| (src, dst, count))
        .collect())
}

fn collect_live_transitions(
    dir: &Path,
    include_expired: bool,
) -> Result<TransitionTriples, Box<dyn Error>> {
    let mut counts = BTreeMap::<(String, String), u64>::new();

    let agent_rows = scan_cf_read_only_with_expired(
        dir,
        SCHEMA_VERSION,
        StorageBackendKind::Calyx,
        cf::CF_AGENT_EVENTS,
        include_expired,
    )?;
    let mut spawn_events = 0_u64;
    for (_key, value) in &agent_rows {
        let record: serde_json::Value = match serde_json::from_slice(value) {
            Ok(record) => record,
            Err(_) => continue,
        };
        if record.get("kind").and_then(serde_json::Value::as_str) != Some("spawn_requested") {
            continue;
        }
        let session = record
            .pointer("/payload/started_by_session_id")
            .and_then(serde_json::Value::as_str)
            .or_else(|| record.get("session_id").and_then(serde_json::Value::as_str))
            .or_else(|| {
                record
                    .pointer("/attributes/conversation_id")
                    .and_then(serde_json::Value::as_str)
            });
        let spawn = record.get("spawn_id").and_then(serde_json::Value::as_str);
        let (Some(session), Some(spawn)) = (session, spawn) else {
            continue;
        };
        if session.trim().is_empty() || spawn.trim().is_empty() {
            continue;
        }
        spawn_events += 1;
        *counts
            .entry((
                format!("agent-session:{session}"),
                format!("agent-spawn:{spawn}"),
            ))
            .or_default() += 1;
    }

    let process_rows = scan_cf_read_only_with_expired(
        dir,
        SCHEMA_VERSION,
        StorageBackendKind::Calyx,
        cf::CF_PROCESS_HISTORY,
        include_expired,
    )?;
    let mut process_edges = 0_u64;
    let mut process_field_census = BTreeMap::<String, u64>::new();
    for (_key, value) in &process_rows {
        if let Ok(serde_json::Value::Object(object)) =
            serde_json::from_slice::<serde_json::Value>(value)
        {
            for field in object.keys() {
                *process_field_census.entry(field.clone()).or_default() += 1;
            }
        }
    }
    for (field, rows) in &process_field_census {
        println!("PROCESS_FIELD name={field} rows={rows}");
    }
    for (_key, value) in &process_rows {
        let record: serde_json::Value = match serde_json::from_slice(value) {
            Ok(record) => record,
            Err(_) => continue,
        };
        let Some(object) = record.as_object() else {
            continue;
        };
        let pid = object.get("pid").and_then(serde_json::Value::as_u64);
        let parent = object
            .get("parent_pid")
            .or_else(|| object.get("ppid"))
            .or_else(|| object.get("inherited_from_pid"))
            .and_then(serde_json::Value::as_u64);
        let (Some(pid), Some(parent)) = (pid, parent) else {
            continue;
        };
        if pid == 0 || parent == 0 || pid == parent {
            continue;
        }
        process_edges += 1;
        *counts
            .entry((format!("process:{parent}"), format!("process:{pid}")))
            .or_default() += 1;
    }

    println!(
        "SOURCE_OF_TRUTH vault={} agent_rows={} spawn_requested_edges={} process_rows={} \
         process_parent_edges={} distinct_pairs={}",
        dir.display(),
        agent_rows.len(),
        spawn_events,
        process_rows.len(),
        process_edges,
        counts.len()
    );
    Ok(counts
        .into_iter()
        .map(|((src, dst), count)| (src, dst, count))
        .collect())
}

fn graph_node_cx_id(name: &str) -> CxId {
    let mut hasher = Sha256::new();
    hasher.update(b"synapse-derived-graph-node-v1");
    hasher.update(PROCESS_IDENTITY_TAG);
    hasher.update((name.len() as u64).to_be_bytes());
    hasher.update(name.as_bytes());
    let digest = hasher.finalize();
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    CxId::from_bytes(bytes)
}

fn report(label: &str, transitions: &[(String, String, u64)]) -> Result<(), Box<dyn Error>> {
    if transitions.is_empty() {
        return Err("no transitions reconstructed; nothing for the spectral pass to act on".into());
    }
    let mut names = BTreeSet::new();
    for (src, dst, _) in transitions {
        names.insert(src.clone());
        names.insert(dst.clone());
    }
    let ids: BTreeMap<String, CxId> = names
        .iter()
        .map(|name| (name.clone(), graph_node_cx_id(name)))
        .collect();
    let edges: Vec<TransitionEdge> = transitions
        .iter()
        .map(|(src, dst, count)| TransitionEdge {
            src: ids[src],
            dst: ids[dst],
            count: *count as f64,
        })
        .collect();

    // Undirected component census over exactly the symmetrization
    // `SymmetricSparseGraph::from_assoc` performs.
    let index: BTreeMap<&String, usize> = names.iter().zip(0..).collect();
    let mut adjacency = vec![Vec::<usize>::new(); names.len()];
    for (src, dst, _) in transitions {
        adjacency[index[src]].push(index[dst]);
        adjacency[index[dst]].push(index[src]);
    }
    let mut seen = vec![false; names.len()];
    let mut components = 0_usize;
    let mut largest = 0_usize;
    let mut singleton_edges = 0_usize;
    for root in 0..names.len() {
        if seen[root] {
            continue;
        }
        components += 1;
        let mut stack = vec![root];
        seen[root] = true;
        let mut size = 0_usize;
        while let Some(node) = stack.pop() {
            size += 1;
            for neighbor in &adjacency[node] {
                if !seen[*neighbor] {
                    seen[*neighbor] = true;
                    stack.push(*neighbor);
                }
            }
        }
        largest = largest.max(size);
        if size == 2 {
            singleton_edges += 1;
        }
    }
    let max_pair = transitions
        .iter()
        .map(|(_, _, count)| *count)
        .max()
        .unwrap_or(1);
    let min_pair = transitions
        .iter()
        .map(|(_, _, count)| *count)
        .min()
        .unwrap_or(1);
    let max_degree = adjacency.iter().map(Vec::len).max().unwrap_or(0);
    println!(
        "GRAPH_SHAPE label={label} nodes={} pairs={} components={} largest_component={} \
         dyad_components={} max_degree={} min_pair_count={min_pair} max_pair_count={max_pair} \
         min_edge_weight={:e} max_edge_weight={:e}",
        names.len(),
        transitions.len(),
        components,
        largest,
        singleton_edges,
        max_degree,
        (min_pair as f64) / (max_pair.max(1) as f64),
        1.0_f64
    );

    let graph = build_transition_graph(&edges)?;
    let params = StructuralParams::default();
    println!(
        "SPECTRAL_PARAMS max_iter={} tol={:e}",
        params.eigenvector_max_iter, params.eigenvector_tol
    );

    // The budget this graph actually demands, measured rather than assumed: the
    // smallest `max_iter` at which the frozen tolerance is reached. Headroom
    // against the frozen 256 is the whole question a `NOT_CONVERGED` raises, and
    // a run that merely says "converged" does not answer it.
    let mut required = None;
    for candidate in [
        1_usize, 2, 4, 8, 16, 32, 64, 128, 256, 512, 1_024, 4_096, 16_384, 65_536,
    ] {
        if eigenvector_centrality(&graph, candidate, params.eigenvector_tol).is_ok() {
            required = Some(candidate);
            break;
        }
    }
    match required {
        Some(iterations) => println!(
            "SPECTRAL_BUDGET required_max_iter<={iterations} frozen_max_iter={} headroom={}",
            params.eigenvector_max_iter,
            if iterations <= params.eigenvector_max_iter {
                "yes"
            } else {
                "NO"
            }
        ),
        None => println!(
            "SPECTRAL_BUDGET required_max_iter>65536 frozen_max_iter={} headroom=NO",
            params.eigenvector_max_iter
        ),
    }
    match eigenvector_centrality(&graph, params.eigenvector_max_iter, params.eigenvector_tol) {
        Ok(scores) => {
            println!("SPECTRAL_OK scored_nodes={}", scores.len());
            let reverse: BTreeMap<CxId, &String> =
                ids.iter().map(|(name, id)| (*id, name)).collect();
            for (id, score) in scores.iter().take(12) {
                println!(
                    "SPECTRAL_TOP node={} eigenvector={score:.8}",
                    reverse.get(id).map_or("<unknown>", |name| name.as_str())
                );
            }
        }
        Err(error) => {
            println!("SPECTRAL_FAILED code={} detail={error}", error.code());
            return Err(format!("eigenvector_centrality refused: {error}").into());
        }
    }

    let structural = structural_signatures(&graph, params)?;
    println!("STRUCTURAL_OK signatures={}", structural.len());
    Ok(())
}
