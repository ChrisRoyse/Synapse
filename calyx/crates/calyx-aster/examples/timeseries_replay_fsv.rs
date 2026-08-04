use std::path::PathBuf;

use calyx_aster::cf::ColumnFamily;
use calyx_aster::collection::{
    Collection, CollectionMode, DedupPolicy, RetentionPolicy, TemporalPolicy, TenantId, TxnPolicy,
    create_collection,
};
use calyx_aster::layers::{RollupWindow, TimeSeriesLayer};
use calyx_aster::vault::{AsterVault, VaultOptions};
use calyx_core::VaultId;
use ulid::Ulid;

fn collection() -> Collection {
    Collection {
        name: "fsv-timeseries-replay".to_owned(),
        mode: CollectionMode::TimeSeries,
        schema: None,
        panel: None,
        indexes: Vec::new(),
        dedup: DedupPolicy::Off,
        temporal: TemporalPolicy::default(),
        retention: RetentionPolicy::Forever,
        txn_policy: TxnPolicy::default(),
        tenant: TenantId::default(),
    }
}

fn state(
    layer: &TimeSeriesLayer<'_, calyx_core::SystemClock>,
    col: &Collection,
    series: u64,
    ts: u64,
) -> calyx_core::Result<String> {
    let points = layer.ts_range(col, series, 0, u64::MAX)?;
    let minute = layer.ts_rollup(col, series, RollupWindow::OneMinute, ts)?;
    let hour = layer.ts_rollup(col, series, RollupWindow::OneHour, ts)?;
    let day = layer.ts_rollup(col, series, RollupWindow::OneDay, ts)?;
    Ok(format!(
        "points={points:?} minute={minute:?} hour={hour:?} day={day:?}"
    ))
}

fn main() -> calyx_core::Result<()> {
    let root: PathBuf =
        std::env::temp_dir().join(format!("synapse-timeseries-replay-fsv-{}", Ulid::new()));
    println!("source_of_truth={}", root.display());
    let vault = AsterVault::open(
        &root,
        VaultId::from_ulid(Ulid::new()),
        b"timeseries-replay-fsv",
        VaultOptions::default(),
    )?;
    let col = collection();
    create_collection(&vault, col.clone())?;
    let layer = TimeSeriesLayer::new(&vault);
    let series = 7_u64;
    let ts = 3_661_000_000_000_u64;

    println!("happy.before {}", state(&layer, &col, series, ts)?);
    let first_seq = layer.ts_write(&col, series, ts, 4.0)?;
    println!(
        "happy.after seq={first_seq} {}",
        state(&layer, &col, series, ts)?
    );

    let replay_before = state(&layer, &col, series, ts)?;
    let replay_seq = layer.ts_write(&col, series, ts, 4.0)?;
    let replay_after = state(&layer, &col, series, ts)?;
    println!(
        "replay.before {replay_before}\nreplay.after seq={replay_seq} {replay_after} unchanged={}",
        replay_before == replay_after && replay_seq == first_seq
    );

    let conflict_before = state(&layer, &col, series, ts)?;
    let conflict = layer.ts_write(&col, series, ts, 9.0).unwrap_err();
    let conflict_after = state(&layer, &col, series, ts)?;
    println!(
        "conflict.before {conflict_before}\nconflict.error code={} message={} remediation={}\nconflict.after {conflict_after} unchanged={}",
        conflict.code,
        conflict.message,
        conflict.remediation,
        conflict_before == conflict_after
    );

    let invalid_before = state(&layer, &col, series, ts)?;
    let invalid = layer.ts_write(&col, series, ts + 1, f64::NAN).unwrap_err();
    let invalid_after = state(&layer, &col, series, ts)?;
    println!(
        "invalid.before {invalid_before}\ninvalid.error code={} message={} remediation={}\ninvalid.after {invalid_after} unchanged={}",
        invalid.code,
        invalid.message,
        invalid.remediation,
        invalid_before == invalid_after
    );

    let next_hour = 7_200_000_000_000_u64;
    println!(
        "boundary.before {}",
        state(&layer, &col, series, next_hour)?
    );
    layer.ts_write(&col, series, next_hour, 6.0)?;
    println!(
        "boundary.after first_window={} second_window={}",
        state(&layer, &col, series, ts)?,
        state(&layer, &col, series, next_hour)?
    );

    let ledger_rows = vault.scan_cf_at(vault.latest_seq(), ColumnFamily::Ledger)?;
    println!(
        "physical_readback latest_seq={} ledger_rows={} ledger_last_key={:02x?}",
        vault.latest_seq(),
        ledger_rows.len(),
        ledger_rows
            .last()
            .map(|row| row.0.as_slice())
            .unwrap_or(&[])
    );
    drop(vault);
    std::fs::remove_dir_all(&root).map_err(|error| calyx_core::CalyxError {
        code: "FSV_CLEANUP_FAILED",
        message: format!("remove {}: {error}", root.display()),
        remediation: "remove the isolated FSV vault manually",
    })?;
    Ok(())
}
