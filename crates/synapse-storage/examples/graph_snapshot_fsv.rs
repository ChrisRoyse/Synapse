//! Manual Full State Verification for atomic graph-position snapshots (#1685).

use std::error::Error;
use std::path::PathBuf;

use calyx_aster::cf::ColumnFamily;
use calyx_core::SlotVector;
use synapse_calyx::{SynapseCalyxConfig, SynapseCalyxVault};
use synapse_storage::constellations::{
    GraphPositionKind, publish_graph_position_snapshot, publish_path_hierarchy_snapshot,
};

fn counts(vault: &SynapseCalyxVault) -> Result<(usize, usize, usize), Box<dyn Error>> {
    Ok((
        vault.scan_cf_latest(ColumnFamily::Graph)?.len(),
        vault.scan_cf_latest(ColumnFamily::Registry)?.len(),
        vault.scan_cf_latest(ColumnFamily::Base)?.len(),
    ))
}

fn main() -> Result<(), Box<dyn Error>> {
    let dir = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .ok_or("usage: graph_snapshot_fsv <new-empty-vault-dir>")?;
    std::fs::create_dir_all(&dir)?;
    let vault = SynapseCalyxVault::open(SynapseCalyxConfig::from_vault_dir(dir.clone()))?;
    println!(
        "SOURCE_OF_TRUTH vault={} dir={}",
        vault.vault_id(),
        dir.display()
    );

    let before = counts(&vault)?;
    println!("EDGE_EMPTY before={before:?}");
    let empty =
        publish_graph_position_snapshot(&vault, GraphPositionKind::App, 40, 1_786_000_000_000, &[]);
    match empty {
        Ok(readback) => {
            return Err(format!("empty graph unexpectedly published: {readback:?}").into());
        }
        Err(error) => println!("EDGE_EMPTY error={error}"),
    }
    println!("EDGE_EMPTY after={:?}", counts(&vault)?);

    let invalid_edges = vec![("leaf".to_owned(), "hub".to_owned(), 0)];
    let before_invalid = counts(&vault)?;
    println!("EDGE_INVALID_COUNT before={before_invalid:?}");
    let invalid = publish_graph_position_snapshot(
        &vault,
        GraphPositionKind::App,
        40,
        1_786_000_000_000,
        &invalid_edges,
    );
    match invalid {
        Ok(readback) => {
            return Err(format!("zero-count graph unexpectedly published: {readback:?}").into());
        }
        Err(error) => println!("EDGE_INVALID_COUNT error={error}"),
    }
    println!("EDGE_INVALID_COUNT after={:?}", counts(&vault)?);

    let edges = vec![
        ("leaf-a".to_owned(), "hub".to_owned(), 1),
        ("leaf-b".to_owned(), "hub".to_owned(), 1),
        ("hub".to_owned(), "leaf-c".to_owned(), 1),
        ("hub".to_owned(), "leaf-d".to_owned(), 1),
    ];
    let happy_before = counts(&vault)?;
    println!("HAPPY before={happy_before:?} expected_nodes=5 expected_edges=4");
    let first = publish_graph_position_snapshot(
        &vault,
        GraphPositionKind::App,
        40,
        1_786_000_000_000,
        &edges,
    )?;
    println!("HAPPY trigger_readback={first:?}");
    let happy_after = counts(&vault)?;
    println!("HAPPY after={happy_after:?}");

    let ids = vault.panel_constellation_ids(first.panel_version, 16)?;
    println!("PHYSICAL panel_ids={ids:?}");
    for id in &ids {
        let cx = vault.hydrate_constellation_latest(*id)?;
        let node = cx
            .metadata
            .get("graph_node_id")
            .cloned()
            .unwrap_or_default();
        let raw_betweenness = cx
            .metadata
            .get("graph_betweenness")
            .cloned()
            .unwrap_or_default();
        let signature = match cx.slots.values().next() {
            Some(SlotVector::Dense { data, .. }) => data.clone(),
            other => return Err(format!("node {node} missing dense signature: {other:?}").into()),
        };
        println!(
            "PHYSICAL node={node} cx_id={} betweenness={raw_betweenness} signature={signature:?}",
            cx.cx_id
        );
    }

    let replay_before = counts(&vault)?;
    println!("EDGE_REPLAY before={replay_before:?}");
    let replay = publish_graph_position_snapshot(
        &vault,
        GraphPositionKind::App,
        40,
        1_786_000_000_000,
        &edges,
    )?;
    println!("EDGE_REPLAY readback={replay:?}");
    println!("EDGE_REPLAY after={:?}", counts(&vault)?);

    let paths = vec![
        "docs/calyx/guide.md".to_owned(),
        "docs/calyx/reference.md".to_owned(),
        "docs/synapse/readme.md".to_owned(),
    ];
    println!("PATH_HAPPY before={:?}", counts(&vault)?);
    let path_readback = publish_path_hierarchy_snapshot(&vault, 41, 1_786_000_000_001, &paths)?;
    println!("PATH_HAPPY trigger_readback={path_readback:?}");
    println!("PATH_HAPPY after={:?}", counts(&vault)?);
    for id in vault.panel_constellation_ids(path_readback.panel_version, 16)? {
        let cx = vault.hydrate_constellation_latest(id)?;
        println!(
            "PATH_PHYSICAL node={} depth={} subtree={} slots={}",
            cx.metadata
                .get("path_node_key")
                .cloned()
                .unwrap_or_default(),
            cx.metadata.get("path_depth").cloned().unwrap_or_default(),
            cx.metadata
                .get("path_subtree_size")
                .cloned()
                .unwrap_or_default(),
            cx.slots.len(),
        );
    }
    Ok(())
}
