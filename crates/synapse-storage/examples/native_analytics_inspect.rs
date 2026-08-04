use std::error::Error;
use std::path::PathBuf;

use calyx_aster::cf::ColumnFamily;
use sha2::{Digest as _, Sha256};
use synapse_calyx::{SynapseCalyxConfig, SynapseCalyxReadOnlyVault};

fn main() -> Result<(), Box<dyn Error>> {
    let vault_dir = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .ok_or("usage: native_analytics_inspect <vault_dir>")?;
    let vault = SynapseCalyxReadOnlyVault::open_existing_with_cfs(
        SynapseCalyxConfig::from_vault_dir(vault_dir),
        Some(vec![ColumnFamily::TimeSeries, ColumnFamily::Collections]),
    )?;
    print_rows("TimeSeries", &vault.scan_timeseries_latest()?);
    print_rows("Collections", &vault.scan_collections_latest()?);
    Ok(())
}

fn print_rows(label: &str, rows: &[(Vec<u8>, Vec<u8>)]) {
    let mut digest = Sha256::new();
    let mut bytes = 0_usize;
    for (key, value) in rows {
        digest.update((key.len() as u64).to_be_bytes());
        digest.update(key);
        digest.update((value.len() as u64).to_be_bytes());
        digest.update(value);
        bytes = bytes.saturating_add(key.len()).saturating_add(value.len());
    }
    let digest = digest.finalize();
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(digest.len() * 2);
    for byte in digest {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    println!(
        "native_family={label} rows={} bytes={} ordered_rows_sha256={encoded}",
        rows.len(),
        bytes
    );
}
