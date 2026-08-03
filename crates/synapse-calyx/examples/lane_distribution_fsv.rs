//! Physical-vault readback for issue #1983's sample/census boundary.
//!
//! Run only against a consistent vault backup: this opens the real Calyx store
//! and proves a bounded hydration is reported as a sample, never a census.

use std::error::Error;
use std::path::PathBuf;

use synapse_calyx::{SynapseCalyxConfig, SynapseCalyxTuningConfig, SynapseCalyxVault};

const PANELS: &[u32] = &[1_665_001, 1_921_001];

fn main() -> Result<(), Box<dyn Error>> {
    let vault_dir = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .ok_or("usage: lane_distribution_fsv <exact-vault-dir>")?;
    let requested_records = std::env::args()
        .nth(2)
        .map(|value| value.parse::<usize>())
        .transpose()?
        .unwrap_or(100_000);
    if !vault_dir.is_dir() {
        return Err(format!("{} is not a directory", vault_dir.display()).into());
    }
    let vault = SynapseCalyxVault::open(SynapseCalyxConfig {
        vault_dir: vault_dir.clone(),
        machine_salt_path: vault_dir
            .parent()
            .ok_or("vault directory has no parent")?
            .join("machine-salt.bin"),
        tuning: SynapseCalyxTuningConfig::default().validate()?,
    })?;

    println!("source_of_truth={}", vault_dir.display());
    println!("latest_seq={}", vault.latest_seq());
    let coverage = vault.lens_coverage_status(PANELS, requested_records)?;
    for panel in &coverage.panels {
        println!(
            "panel={} observed={}/{} census_complete={}",
            panel.panel_version,
            panel.records_measured,
            panel.records_scanned,
            panel.records_measured == panel.records_scanned
        );
    }
    for lane in &coverage.degenerate_lanes {
        println!(
            "panel={} slot={} code={} observed={}/{} distinct={} frequency_ratio={:?} percent_unique={:.9} census_complete={}",
            lane.panel_version,
            lane.slot,
            lane.code,
            lane.records_present,
            lane.population_records,
            lane.distinct_values,
            lane.frequency_ratio,
            lane.percent_unique,
            lane.census_complete
        );
    }

    if requested_records < 8 {
        if !coverage.degenerate_lanes.is_empty() {
            return Err("below-floor sample produced a distribution verdict".into());
        }
        println!("verdict=PASS below-floor sample produced no distribution claim");
        return Ok(());
    }
    let transcript = coverage
        .degenerate_lanes
        .iter()
        .find(|lane| lane.panel_version == 1_921_001 && lane.slot == 36)
        .ok_or("panel 1921001 slot 36 was not classified")?;
    if transcript.code != "CALYX_LENS_CONSTANT_BY_SAMPLE"
        || transcript.census_complete
        || transcript.population_records <= transcript.records_present
    {
        return Err(format!("unexpected transcript verdict: {transcript:?}").into());
    }
    println!("verdict=PASS bounded physical evidence remained sample-qualified");
    Ok(())
}
