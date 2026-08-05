//! Manual FSV for #1883 retired decorative Calyx tuning keys.
//!
//! Usage: `cargo run -p synapse-calyx --example retired_tuning_config_fsv -- <scratch-dir>`

use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use synapse_calyx::SynapseCalyxConfig;

const RETIRED: &[&str] = &[
    "bit_floor_bits",
    "correlation_ceiling",
    "guard_cold_start_tau",
    "kernel_fraction",
    "kernel_recall_gate",
    "temporal_boost_min",
    "temporal_boost_max",
];

fn hash(path: &Path) -> Result<String, Box<dyn Error>> {
    let digest = Sha256::digest(fs::read(path)?);
    Ok(digest.iter().fold(String::new(), |mut text, byte| {
        use std::fmt::Write as _;
        let _ = write!(text, "{byte:02x}");
        text
    }))
}

fn main() -> Result<(), Box<dyn Error>> {
    let dir = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .ok_or("usage: retired_tuning_config_fsv <scratch-dir>")?;
    fs::create_dir_all(&dir)?;
    let config_path = dir.join("calyx.toml");
    let vault_dir = dir.join("vault");

    fs::write(&config_path, "[calyx]\nfusion_k = 5\n")?;
    let valid_before = hash(&config_path)?;
    let config = SynapseCalyxConfig::from_optional_vault_dir_and_config_path(
        Some(vault_dir.clone()),
        Some(config_path.clone()),
    )?;
    let valid_after = hash(&config_path)?;
    println!(
        "HAPPY path={} before_sha256={} after_sha256={} fusion_k={}",
        config_path.display(),
        valid_before,
        valid_after,
        config.tuning.fusion_k
    );
    if valid_before != valid_after || config.tuning.fusion_k != 5 {
        return Err("valid load-bearing tuning did not round-trip byte-identically".into());
    }

    for (index, key) in RETIRED.iter().enumerate() {
        let value = if key.contains("boost")
            || key.contains("fraction")
            || key.contains("ceiling")
            || key.contains("tau")
            || key.contains("floor")
            || key.contains("recall")
        {
            "0.5"
        } else {
            "1"
        };
        fs::write(&config_path, format!("[calyx]\n{key} = {value}\n"))?;
        let before = hash(&config_path)?;
        let error = match SynapseCalyxConfig::from_optional_vault_dir_and_config_path(
            Some(vault_dir.clone()),
            Some(config_path.clone()),
        ) {
            Ok(_) => return Err(format!("retired key {key} unexpectedly parsed").into()),
            Err(error) => error,
        };
        let after = hash(&config_path)?;
        println!(
            "EDGE {} key={} before_sha256={} code={} error={} after_sha256={}",
            index + 1,
            key,
            before,
            error.code,
            error,
            after
        );
        if error.code != "SYNAPSE_CALYX_CONFIG_PARSE_FAILED" || before != after {
            return Err(format!("retired key {key} did not fail closed byte-identically").into());
        }
    }
    println!(
        "FINAL retired={} config_sha256={}",
        RETIRED.len(),
        hash(&config_path)?
    );
    Ok(())
}
