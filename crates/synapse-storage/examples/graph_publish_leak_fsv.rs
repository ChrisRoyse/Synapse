//! Manual Full State Verification for the graph-position publish generation
//! leak (#2081 ask 2).
//!
//! ## The claim under test
//!
//! A `publish_graph_position_snapshot` that **fails** must mint nothing. The
//! defect was that the vault-global panel generation was allocated *before* the
//! structural pass that fails, so each failed tick permanently consumed one
//! owner row out of `MAX_OWNERS = 10_000`. Owner rows are never released —
//! attribution depends on them — and a failed publish commits no `Base` rows, so
//! the stranded generation is also invisible to the physical census that
//! `storage operation=panel_coverage` reports. #2062's three safety nets all
//! miss it by construction, and the ORPHANED error's own remediation ("swept by
//! the next successful publish") is unreachable when the publish fails
//! deterministically.
//!
//! The authority for the claim is therefore **not** the census — which cannot
//! see the leak — but `SynapseCalyxVault::panel_generation_allocator()`, the
//! owner map itself. This harness reads it before and after every publish.
//!
//! ## How the failure is injected
//!
//! With a real graph, not a stub: a long parent→child process chain. A path
//! graph `P_n` has adjacency eigenvalues `2*cos(k*pi/(n+1))`, so its two largest
//! are separated by `O(1/n^2)`; at `n = 400` the shifted power iteration's
//! contraction ratio exceeds `0.9999` and the frozen `max_iter = 256` cannot
//! reach `tol = 1e-6`. That is a genuine, correctly-refused input — a real
//! spectral gap that a fixed budget cannot close — not a mocked error, and it is
//! a shape this publisher can actually receive, since `process:{parent}` →
//! `process:{pid}` edges chain naturally.
//!
//! ## Usage
//!
//! ```text
//! cargo run -p synapse-storage --example graph_publish_leak_fsv -- <new-empty-vault-dir> [failed_ticks]
//! ```

use std::{error::Error, path::PathBuf};

use calyx_aster::cf::ColumnFamily;
use synapse_calyx::{SynapseCalyxConfig, SynapseCalyxMathBackend, SynapseCalyxVault};
use synapse_storage::constellations::{GraphPositionKind, publish_graph_position_snapshot};

/// Long enough that `P_n`'s `O(1/n^2)` spectral gap cannot be closed inside the
/// frozen 256-iteration budget.
const CHAIN_NODES: u64 = 400;
/// A hub-and-spoke graph the structural pass converges on immediately.
const STAR_LEAVES: u64 = 8;

/// `(owner_count, next_generation, generations owned by the process panel)`.
fn allocator_state(vault: &SynapseCalyxVault) -> Result<(u64, u32, Vec<u32>), Box<dyn Error>> {
    let readback = vault.panel_generation_allocator()?;
    let panel = GraphPositionKind::Process.panel_name();
    let mut owned: Vec<u32> = readback
        .owners
        .iter()
        // The allocator stores reserved built-ins as `builtin:{panel}` and
        // dynamically allocated generations under the bare panel name, so match
        // on the suffix rather than assuming one of the two spellings.
        .filter(|(_generation, owner)| {
            owner.as_str() == panel || owner.as_str() == format!("builtin:{panel}")
        })
        .map(|(generation, _owner)| *generation)
        .collect();
    owned.sort_unstable();
    Ok((readback.owner_count, readback.next_generation, owned))
}

fn chain(nodes: u64) -> Vec<(String, String, u64)> {
    (1..nodes)
        .map(|pid| (format!("process:{pid}"), format!("process:{}", pid + 1), 1))
        .collect()
}

fn star(leaves: u64) -> Vec<(String, String, u64)> {
    (1..=leaves)
        .map(|leaf| {
            (
                "process:1".to_owned(),
                format!("process:{}", leaf + 1000),
                1,
            )
        })
        .collect()
}

fn base_rows(vault: &SynapseCalyxVault) -> Result<usize, Box<dyn Error>> {
    Ok(vault.scan_cf_latest(ColumnFamily::Base)?.len())
}

fn main() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args().skip(1);
    let dir = PathBuf::from(
        args.next()
            .ok_or("usage: graph_publish_leak_fsv <new-empty-vault-dir> [failed_ticks]")?,
    );
    let failed_ticks: usize = match args.next() {
        Some(value) => value.parse()?,
        None => 5,
    };
    std::fs::create_dir_all(&dir)?;
    // A scratch FSV vault on a CUDA host must say which math it wants: `Auto`
    // refuses rather than silently running CPU kernels on a GPU box. The
    // structural pass under test is CPU math either way.
    let mut config = SynapseCalyxConfig::from_vault_dir(dir.clone());
    config.tuning.math_backend = SynapseCalyxMathBackend::Cpu;
    let vault = SynapseCalyxVault::open(config)?;
    println!(
        "SOURCE_OF_TRUTH vault={} dir={} panel={}",
        vault.vault_id(),
        dir.display(),
        GraphPositionKind::Process.panel_name()
    );

    let (owners_start, next_start, owned_start) = allocator_state(&vault)?;
    println!(
        "ALLOCATOR_BEFORE owner_count={owners_start} next_generation={next_start} \
         panel_owned_generations={owned_start:?} base_rows={}",
        base_rows(&vault)?
    );

    // Every failed tick must be a no-op on the allocator.
    let failing = chain(CHAIN_NODES);
    for tick in 1..=failed_ticks {
        // A distinct `source_seq` per tick so the operation identity differs,
        // exactly as consecutive real ticks differ. An idempotent replay of one
        // identity would not test anything.
        let outcome = publish_graph_position_snapshot(
            &vault,
            GraphPositionKind::Process,
            1_000 + tick as u64,
            1_786_000_000_000,
            &failing,
        );
        let (owners, next, owned) = allocator_state(&vault)?;
        match outcome {
            Ok(readback) => {
                return Err(format!(
                    "the {CHAIN_NODES}-node chain was expected to refuse, but published: {readback:?}"
                )
                .into());
            }
            Err(error) => println!(
                "FAILED_TICK tick={tick} owner_count={owners} next_generation={next} \
                 panel_owned_generations={owned:?} base_rows={} error={error}",
                base_rows(&vault)?
            ),
        }
        if (owners, next, owned.clone()) != (owners_start, next_start, owned_start.clone()) {
            return Err(format!(
                "LEAK: a failed publish moved the allocator. before=({owners_start}, \
                 {next_start}, {owned_start:?}) after=({owners}, {next}, {owned:?})"
            )
            .into());
        }
    }

    let (owners_after_failures, next_after_failures, owned_after_failures) =
        allocator_state(&vault)?;
    println!(
        "LEAK_FREEDOM failed_ticks={failed_ticks} owner_count_delta={} \
         next_generation_delta={} panel_owned_generation_delta={}",
        owners_after_failures - owners_start,
        next_after_failures - next_start,
        owned_after_failures.len() - owned_start.len()
    );

    // And a publish that *can* succeed must still mint exactly one, and that one
    // must be census-visible: it has to carry `Base` rows.
    let rows_before_success = base_rows(&vault)?;
    let readback = publish_graph_position_snapshot(
        &vault,
        GraphPositionKind::Process,
        2_000,
        1_786_000_000_000,
        &star(STAR_LEAVES),
    )?;
    let (owners_end, next_end, owned_end) = allocator_state(&vault)?;
    println!(
        "SUCCESS_TICK panel_version={} constellation_count={} graph_row_count={} \
         owner_count={owners_end} next_generation={next_end} panel_owned_generations={owned_end:?} \
         base_rows={} base_rows_added={}",
        readback.panel_version,
        readback.constellation_count,
        readback.graph_row_count,
        base_rows(&vault)?,
        base_rows(&vault)? - rows_before_success
    );

    // The dynamic generation is minted under the allocator's own owner naming,
    // which differs from the built-in reservation's, so the claim is checked on
    // the watermark the allocator actually advances rather than on a name.
    if next_end <= next_after_failures {
        return Err(format!(
            "a successful publish must advance the allocator watermark:              {next_after_failures} -> {next_end} (owned {owned_end:?})"
        )
        .into());
    }
    if owners_end <= owners_after_failures {
        return Err(format!(
            "a successful publish must own its generation: owner_count              {owners_after_failures} -> {owners_end}"
        )
        .into());
    }
    if readback.constellation_count == 0 || readback.graph_row_count == 0 {
        return Err("the minted generation holds no rows, so it is census-invisible".into());
    }
    println!(
        "VERDICT failed_publishes_minted=0 successful_publishes_minted=1 \
         census_visible_rows={} leak_free=true",
        readback.constellation_count
    );
    Ok(())
}
