use std::{
    ffi::OsString,
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    process,
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use synapse_core::retention::{DEFAULTS, RetentionDefault, RetentionTtl};

use crate::{
    Db, RawRow, StorageBackendKind, StorageError, StorageResult,
    agent_events::decode_agent_event_key,
    backend::{CalyxMigrationRow, scan_calyx_cf_read_only_including_expired},
    cf,
    episodes::decode_episode_key,
    scan_cf_read_only,
    timeline::decode_timeline_key,
};

pub const DEFAULT_MIGRATION_BATCH_ROWS: usize = 4096;
const MIGRATION_MANIFEST_SCHEMA_VERSION: u32 = 1;
const NANOS_PER_MILLI: u64 = 1_000_000;
const NANOS_PER_SECOND: u64 = 1_000_000_000;
const SECONDS_PER_HOUR: u64 = 60 * 60;
const HOURS_PER_DAY: u64 = 24;
const TS_NS_FIELD: &[u8] = br#""ts_ns""#;

#[derive(Clone, Debug)]
pub struct StorageMigrationConfig {
    pub source_rocksdb_path: PathBuf,
    pub target_calyx_path: PathBuf,
    pub manifest_path: PathBuf,
    pub schema_version: u32,
    pub batch_rows: usize,
    pub rename_source_on_success: bool,
    pub retired_rocksdb_path: Option<PathBuf>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StorageMigrationManifest {
    pub manifest_schema_version: u32,
    pub storage_schema_version: u32,
    pub source_backend: String,
    pub target_backend: String,
    pub source_rocksdb_path: String,
    pub target_calyx_path: String,
    pub manifest_path: String,
    pub started_at_unix_ms: u64,
    pub completed_at_unix_ms: u64,
    pub batch_rows: u64,
    pub total_rows: u64,
    pub total_key_bytes: u64,
    pub total_value_bytes: u64,
    pub source_digest_sha256: String,
    pub target_digest_sha256: String,
    pub cf_reports: Vec<StorageMigrationCfReport>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retired_rocksdb_path: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StorageMigrationCfReport {
    pub cf_name: String,
    pub source_rows: u64,
    pub target_rows: u64,
    pub rows_written: u64,
    pub source_key_bytes: u64,
    pub source_value_bytes: u64,
    pub target_key_bytes: u64,
    pub target_value_bytes: u64,
    pub source_digest_sha256: String,
    pub target_digest_sha256: String,
    pub verified_byte_exact: bool,
    pub ttl_policy: String,
    pub source_timestamped_rows: u64,
    pub source_timestamp_missing_rows: u64,
    pub expires_at_zero_rows: u64,
    pub expired_at_migration_rows: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_key_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_key_sha256: Option<String>,
}

#[derive(Debug)]
struct RowSummary {
    rows: u64,
    key_bytes: u64,
    value_bytes: u64,
    digest_sha256: String,
    first_key_sha256: Option<String>,
    last_key_sha256: Option<String>,
}

#[derive(Debug)]
struct CfMigrationOutcome {
    report: StorageMigrationCfReport,
    source_summary: RowSummary,
    target_summary: RowSummary,
}

#[derive(Debug)]
struct MigrationTotals {
    total_rows: u64,
    total_key_bytes: u64,
    total_value_bytes: u64,
    source_digest_sha256: String,
    target_digest_sha256: String,
}

#[derive(Debug)]
struct RowEnvelopeTiming {
    written_at_ms: u64,
    expires_at_ms: u64,
    had_source_timestamp: bool,
}

#[derive(Debug)]
struct RetentionMigrationSummary {
    ttl_policy: String,
    source_timestamped_rows: u64,
    source_timestamp_missing_rows: u64,
    expires_at_zero_rows: u64,
    expired_at_migration_rows: u64,
}

#[derive(Debug)]
struct StagedManifest {
    path: PathBuf,
    bytes: Vec<u8>,
    sha256: String,
}

impl RetentionMigrationSummary {
    fn new(cf_name: &str) -> StorageResult<Self> {
        Ok(Self {
            ttl_policy: ttl_policy_string(retention_default_for_cf(cf_name)?),
            source_timestamped_rows: 0,
            source_timestamp_missing_rows: 0,
            expires_at_zero_rows: 0,
            expired_at_migration_rows: 0,
        })
    }

    const fn record(&mut self, timing: &RowEnvelopeTiming, migration_now_ms: u64) {
        if timing.had_source_timestamp {
            self.source_timestamped_rows = self.source_timestamped_rows.saturating_add(1);
        } else {
            self.source_timestamp_missing_rows =
                self.source_timestamp_missing_rows.saturating_add(1);
        }
        if timing.expires_at_ms == 0 {
            self.expires_at_zero_rows = self.expires_at_zero_rows.saturating_add(1);
        } else if migration_now_ms >= timing.expires_at_ms {
            self.expired_at_migration_rows = self.expired_at_migration_rows.saturating_add(1);
        }
    }
}

/// Streams every known `RocksDB` column family into a Calyx vault and verifies
/// byte-exact logical row parity before writing a manifest.
///
/// # Errors
///
/// Returns a storage error when either backend cannot be opened, any row cannot
/// be read or written, post-write verification finds a key/value mismatch, the
/// manifest cannot be durably written, or the optional source rename fails.
pub fn migrate_rocksdb_to_calyx(
    config: &StorageMigrationConfig,
) -> StorageResult<StorageMigrationManifest> {
    validate_migration_config(config)?;
    let batch_rows = if config.batch_rows == 0 {
        DEFAULT_MIGRATION_BATCH_ROWS
    } else {
        config.batch_rows
    };
    let started_at_unix_ms = now_unix_ms();
    tracing::info!(
        code = "STORAGE_MIGRATION_START",
        source_rocksdb_path = %config.source_rocksdb_path.display(),
        target_calyx_path = %config.target_calyx_path.display(),
        manifest_path = %config.manifest_path.display(),
        batch_rows,
        rename_source_on_success = config.rename_source_on_success,
        "starting RocksDB to Calyx storage migration"
    );
    let target = Db::open_with_backend(
        &config.target_calyx_path,
        config.schema_version,
        StorageBackendKind::Calyx,
    )?;

    let mut cf_reports = Vec::with_capacity(cf::ALL_COLUMN_FAMILIES.len());
    let mut source_total = Sha256::new();
    let mut target_total = Sha256::new();
    let mut total_rows = 0_u64;
    let mut total_key_bytes = 0_u64;
    let mut total_value_bytes = 0_u64;

    for cf_name in cf::ALL_COLUMN_FAMILIES {
        let outcome = migrate_one_cf(config, &target, cf_name, batch_rows, started_at_unix_ms)?;
        update_total_digest(
            &mut source_total,
            cf_name,
            &outcome.source_summary.digest_sha256,
        );
        update_total_digest(
            &mut target_total,
            cf_name,
            &outcome.target_summary.digest_sha256,
        );
        total_rows = total_rows
            .checked_add(outcome.source_summary.rows)
            .ok_or_else(|| migration_read_failed(cf_name, "total row count overflow"))?;
        total_key_bytes = total_key_bytes
            .checked_add(outcome.source_summary.key_bytes)
            .ok_or_else(|| migration_read_failed(cf_name, "total key byte count overflow"))?;
        total_value_bytes = total_value_bytes
            .checked_add(outcome.source_summary.value_bytes)
            .ok_or_else(|| migration_read_failed(cf_name, "total value byte count overflow"))?;
        cf_reports.push(outcome.report);
    }

    let totals = finalize_migration_totals(
        source_total,
        target_total,
        total_rows,
        total_key_bytes,
        total_value_bytes,
    )?;
    let retired_rocksdb_path = if config.rename_source_on_success {
        Some(resolved_retired_rocksdb_path(config))
    } else {
        None
    };
    let manifest = build_migration_manifest(
        config,
        started_at_unix_ms,
        batch_rows,
        totals,
        cf_reports,
        retired_rocksdb_path.as_ref(),
    );
    commit_manifest_and_retire_source(config, &manifest, retired_rocksdb_path.as_ref())?;
    tracing::info!(
        code = "STORAGE_MIGRATION_COMPLETE",
        source_rocksdb_path = %config.source_rocksdb_path.display(),
        target_calyx_path = %config.target_calyx_path.display(),
        manifest_path = %config.manifest_path.display(),
        total_rows = manifest.total_rows,
        source_digest_sha256 = %manifest.source_digest_sha256,
        target_digest_sha256 = %manifest.target_digest_sha256,
        retired_rocksdb_path = manifest.retired_rocksdb_path.as_deref().unwrap_or(""),
        "completed RocksDB to Calyx storage migration with byte-exact verification"
    );
    Ok(manifest)
}

fn finalize_migration_totals(
    source_total: Sha256,
    target_total: Sha256,
    total_rows: u64,
    total_key_bytes: u64,
    total_value_bytes: u64,
) -> StorageResult<MigrationTotals> {
    let source_digest_sha256 = format!("sha256:{}", hex_lower(&source_total.finalize()));
    let target_digest_sha256 = format!("sha256:{}", hex_lower(&target_total.finalize()));
    if source_digest_sha256 != target_digest_sha256 {
        tracing::error!(
            code = "STORAGE_MIGRATION_TOTAL_DIGEST_MISMATCH",
            source_digest_sha256,
            target_digest_sha256,
            "migration verification total digest mismatch after per-CF verification"
        );
        return Err(StorageError::ReadFailed {
            cf_name: "<migration-total>".to_owned(),
            detail: format!(
                "migration total digest mismatch: source={source_digest_sha256} target={target_digest_sha256}"
            ),
        });
    }
    Ok(MigrationTotals {
        total_rows,
        total_key_bytes,
        total_value_bytes,
        source_digest_sha256,
        target_digest_sha256,
    })
}

fn build_migration_manifest(
    config: &StorageMigrationConfig,
    started_at_unix_ms: u64,
    batch_rows: usize,
    totals: MigrationTotals,
    cf_reports: Vec<StorageMigrationCfReport>,
    retired_rocksdb_path: Option<&PathBuf>,
) -> StorageMigrationManifest {
    StorageMigrationManifest {
        manifest_schema_version: MIGRATION_MANIFEST_SCHEMA_VERSION,
        storage_schema_version: config.schema_version,
        source_backend: StorageBackendKind::RocksDb.as_str().to_owned(),
        target_backend: StorageBackendKind::Calyx.as_str().to_owned(),
        source_rocksdb_path: config.source_rocksdb_path.display().to_string(),
        target_calyx_path: config.target_calyx_path.display().to_string(),
        manifest_path: config.manifest_path.display().to_string(),
        started_at_unix_ms,
        completed_at_unix_ms: now_unix_ms(),
        batch_rows: batch_rows as u64,
        total_rows: totals.total_rows,
        total_key_bytes: totals.total_key_bytes,
        total_value_bytes: totals.total_value_bytes,
        source_digest_sha256: totals.source_digest_sha256,
        target_digest_sha256: totals.target_digest_sha256,
        cf_reports,
        retired_rocksdb_path: retired_rocksdb_path.map(|path| path.display().to_string()),
    }
}

fn commit_manifest_and_retire_source(
    config: &StorageMigrationConfig,
    manifest: &StorageMigrationManifest,
    retired_rocksdb_path: Option<&PathBuf>,
) -> StorageResult<()> {
    let staged_manifest = stage_manifest(&config.manifest_path, manifest)?;
    if let Some(retired) = retired_rocksdb_path
        && let Err(error) = rename_verified_source(&config.source_rocksdb_path, retired)
    {
        tracing::error!(
            code = "STORAGE_MIGRATION_SOURCE_RETIRE_FAILED",
            source_rocksdb_path = %config.source_rocksdb_path.display(),
            retired_rocksdb_path = %retired.display(),
            manifest_staging_path = %staged_manifest.path.display(),
            manifest_staging_sha256 = %staged_manifest.sha256,
            error = %error,
            "failed to retire verified RocksDB source after staging migration manifest"
        );
        return Err(error);
    }
    commit_manifest(&config.manifest_path, manifest, &staged_manifest)
}

fn validate_migration_config(config: &StorageMigrationConfig) -> StorageResult<()> {
    if !config.source_rocksdb_path.is_dir() {
        return Err(StorageError::OpenFailed {
            path: config.source_rocksdb_path.clone(),
            detail: "source RocksDB path is not an existing directory".to_owned(),
        });
    }
    if config.source_rocksdb_path == config.target_calyx_path {
        return Err(StorageError::BackendInvalidConfig {
            value: config.target_calyx_path.display().to_string(),
            detail: "source RocksDB path and target Calyx path must be different".to_owned(),
        });
    }
    if config.rename_source_on_success
        && config
            .manifest_path
            .starts_with(&config.source_rocksdb_path)
    {
        return Err(StorageError::BackendInvalidConfig {
            value: config.manifest_path.display().to_string(),
            detail: "manifest_path must not live under source_rocksdb_path when rename_source_on_success=true".to_owned(),
        });
    }
    if let Some(retired) = &config.retired_rocksdb_path
        && retired.exists()
    {
        return Err(StorageError::BackendInvalidConfig {
            value: retired.display().to_string(),
            detail: "retired_rocksdb_path already exists; refusing to overwrite".to_owned(),
        });
    }
    Ok(())
}

fn migrate_one_cf(
    config: &StorageMigrationConfig,
    target: &Db,
    cf_name: &str,
    batch_rows: usize,
    migration_now_ms: u64,
) -> StorageResult<CfMigrationOutcome> {
    let source_rows = scan_cf_read_only(
        &config.source_rocksdb_path,
        config.schema_version,
        StorageBackendKind::RocksDb,
        cf_name,
    )?;
    let source_summary = summarize_rows(cf_name, &source_rows)?;
    let mut retention_summary = RetentionMigrationSummary::new(cf_name)?;
    for chunk in source_rows.chunks(batch_rows) {
        let mut rows = Vec::with_capacity(chunk.len());
        for (key, value) in chunk {
            let timing = row_envelope_timing(cf_name, key, value)?;
            retention_summary.record(&timing, migration_now_ms);
            rows.push(CalyxMigrationRow {
                key: key.clone(),
                value: value.clone(),
                written_at_ms: timing.written_at_ms,
                expires_at_ms: timing.expires_at_ms,
            });
        }
        target.put_calyx_migration_batch_pressure_bypass(cf_name, rows)?;
    }
    target.flush()?;
    let target_rows = scan_calyx_cf_read_only_including_expired(
        &config.target_calyx_path,
        config.schema_version,
        cf_name,
    )?;
    verify_rows_equal(cf_name, &source_rows, &target_rows)?;
    let target_summary = summarize_rows(cf_name, &target_rows)?;
    let report = StorageMigrationCfReport {
        cf_name: cf_name.to_owned(),
        source_rows: source_summary.rows,
        target_rows: target_summary.rows,
        rows_written: source_summary.rows,
        source_key_bytes: source_summary.key_bytes,
        source_value_bytes: source_summary.value_bytes,
        target_key_bytes: target_summary.key_bytes,
        target_value_bytes: target_summary.value_bytes,
        source_digest_sha256: source_summary.digest_sha256.clone(),
        target_digest_sha256: target_summary.digest_sha256.clone(),
        verified_byte_exact: true,
        ttl_policy: retention_summary.ttl_policy,
        source_timestamped_rows: retention_summary.source_timestamped_rows,
        source_timestamp_missing_rows: retention_summary.source_timestamp_missing_rows,
        expires_at_zero_rows: retention_summary.expires_at_zero_rows,
        expired_at_migration_rows: retention_summary.expired_at_migration_rows,
        first_key_sha256: source_summary.first_key_sha256.clone(),
        last_key_sha256: source_summary.last_key_sha256.clone(),
    };
    tracing::info!(
        code = "STORAGE_MIGRATION_CF_VERIFIED",
        cf_name,
        source_rows = report.source_rows,
        target_rows = report.target_rows,
        rows_written = report.rows_written,
        source_digest_sha256 = %report.source_digest_sha256,
        target_digest_sha256 = %report.target_digest_sha256,
        source_timestamped_rows = report.source_timestamped_rows,
        source_timestamp_missing_rows = report.source_timestamp_missing_rows,
        expired_at_migration_rows = report.expired_at_migration_rows,
        "verified migrated column family byte-for-byte"
    );
    Ok(CfMigrationOutcome {
        report,
        source_summary,
        target_summary,
    })
}

fn summarize_rows(cf_name: &str, rows: &[RawRow]) -> StorageResult<RowSummary> {
    let mut digest = Sha256::new();
    let mut key_bytes = 0_u64;
    let mut value_bytes = 0_u64;
    for (key, value) in rows {
        key_bytes = key_bytes
            .checked_add(key.len() as u64)
            .ok_or_else(|| migration_read_failed(cf_name, "key byte count overflow"))?;
        value_bytes = value_bytes
            .checked_add(value.len() as u64)
            .ok_or_else(|| migration_read_failed(cf_name, "value byte count overflow"))?;
        update_row_digest(&mut digest, cf_name, key, value);
    }
    Ok(RowSummary {
        rows: rows.len() as u64,
        key_bytes,
        value_bytes,
        digest_sha256: format!("sha256:{}", hex_lower(&digest.finalize())),
        first_key_sha256: rows.first().map(|(key, _value)| row_part_sha(key)),
        last_key_sha256: rows.last().map(|(key, _value)| row_part_sha(key)),
    })
}

fn verify_rows_equal(cf_name: &str, source: &[RawRow], target: &[RawRow]) -> StorageResult<()> {
    if source.len() != target.len() {
        tracing::error!(
            code = "STORAGE_MIGRATION_VERIFY_ROW_COUNT_MISMATCH",
            cf_name,
            source_rows = source.len(),
            target_rows = target.len(),
            "migration verification row-count mismatch"
        );
        return Err(migration_read_failed(
            cf_name,
            &format!(
                "migration verification row-count mismatch: source={} target={}",
                source.len(),
                target.len()
            ),
        ));
    }
    for (index, ((source_key, source_value), (target_key, target_value))) in
        source.iter().zip(target).enumerate()
    {
        if source_key != target_key || source_value != target_value {
            tracing::error!(
                code = "STORAGE_MIGRATION_VERIFY_ROW_MISMATCH",
                cf_name,
                row_index = index,
                source_key_len = source_key.len(),
                source_key_sha256 = %row_part_sha(source_key),
                target_key_len = target_key.len(),
                target_key_sha256 = %row_part_sha(target_key),
                source_value_len = source_value.len(),
                source_value_sha256 = %row_part_sha(source_value),
                target_value_len = target_value.len(),
                target_value_sha256 = %row_part_sha(target_value),
                "migration verification key/value mismatch"
            );
            return Err(migration_read_failed(
                cf_name,
                &format!(
                    "migration verification mismatch at row_index={index}: source_key_len={} source_key_sha256={} target_key_len={} target_key_sha256={} source_value_len={} source_value_sha256={} target_value_len={} target_value_sha256={}",
                    source_key.len(),
                    row_part_sha(source_key),
                    target_key.len(),
                    row_part_sha(target_key),
                    source_value.len(),
                    row_part_sha(source_value),
                    target_value.len(),
                    row_part_sha(target_value)
                ),
            ));
        }
    }
    Ok(())
}

fn update_row_digest(hasher: &mut Sha256, cf_name: &str, key: &[u8], value: &[u8]) {
    update_len_prefixed(hasher, cf_name.as_bytes());
    update_len_prefixed(hasher, key);
    update_len_prefixed(hasher, value);
}

fn update_total_digest(hasher: &mut Sha256, cf_name: &str, digest: &str) {
    update_len_prefixed(hasher, cf_name.as_bytes());
    update_len_prefixed(hasher, digest.as_bytes());
}

fn update_len_prefixed(hasher: &mut Sha256, bytes: &[u8]) {
    hasher.update((bytes.len() as u64).to_be_bytes());
    hasher.update(bytes);
}

fn row_envelope_timing(
    cf_name: &str,
    key: &[u8],
    value: &[u8],
) -> StorageResult<RowEnvelopeTiming> {
    let retention = retention_default_for_cf(cf_name)?;
    let timestamp_ns = source_row_timestamp_ns(cf_name, key, value)?;
    let had_source_timestamp = timestamp_ns.is_some();
    let written_at_ms = timestamp_ns.map_or(0, |ts_ns| ts_ns / NANOS_PER_MILLI);
    let expires_at_ms = match (timestamp_ns, ttl_nanos_for_policy(cf_name, retention.ttl)?) {
        (Some(ts_ns), Some(ttl_ns)) => ts_ns
            .checked_add(ttl_ns)
            .ok_or_else(|| {
                migration_read_failed(
                    cf_name,
                    &format!("migration TTL overflow: ts_ns={ts_ns} ttl_ns={ttl_ns}"),
                )
            })?
            .div_ceil(NANOS_PER_MILLI),
        (_, None) | (None, Some(_)) => 0,
    };
    Ok(RowEnvelopeTiming {
        written_at_ms,
        expires_at_ms,
        had_source_timestamp,
    })
}

fn source_row_timestamp_ns(cf_name: &str, key: &[u8], value: &[u8]) -> StorageResult<Option<u64>> {
    let key_ts_ns = key_timestamp_ns(cf_name, key)?;
    let value_ts_ns = extract_ts_ns(value);
    if let Some(ts_ns) = key_ts_ns.or(value_ts_ns)
        && ts_ns == 0
    {
        return Err(migration_read_failed(
            cf_name,
            "migration source timestamp must be positive when present",
        ));
    }
    if let (Some(key_ts), Some(value_ts)) = (key_ts_ns, value_ts_ns)
        && key_ts != value_ts
        && requires_key_value_timestamp_parity(cf_name)
    {
        return Err(migration_read_failed(
            cf_name,
            &format!(
                "migration source timestamp mismatch: key_ts_ns={key_ts} value_ts_ns={value_ts}"
            ),
        ));
    }
    Ok(value_ts_ns.or(key_ts_ns))
}

fn key_timestamp_ns(cf_name: &str, key: &[u8]) -> StorageResult<Option<u64>> {
    match cf_name {
        cf::CF_TIMELINE => decode_timeline_key(key).map(|(ts_ns, _seq)| Some(ts_ns)),
        cf::CF_EPISODES => decode_episode_key(key).map(|(ts_ns, _seq)| Some(ts_ns)),
        cf::CF_AGENT_EVENTS => decode_agent_event_key(key).map(|(ts_ns, _seq)| Some(ts_ns)),
        cf::CF_EVENTS | cf::CF_ACTION_LOG | cf::CF_REFLEX_AUDIT if key.len() >= 8 => {
            let mut bytes = [0_u8; 8];
            bytes.copy_from_slice(&key[..8]);
            Ok(Some(u64::from_be_bytes(bytes)))
        }
        _ => Ok(None),
    }
}

fn requires_key_value_timestamp_parity(cf_name: &str) -> bool {
    matches!(
        cf_name,
        cf::CF_TIMELINE | cf::CF_EPISODES | cf::CF_AGENT_EVENTS
    )
}

fn extract_ts_ns(value: &[u8]) -> Option<u64> {
    let field_start = value
        .windows(TS_NS_FIELD.len())
        .position(|window| window == TS_NS_FIELD)?;
    let mut index = field_start + TS_NS_FIELD.len();
    index = skip_json_ws(value, index);
    if value.get(index) != Some(&b':') {
        return None;
    }
    index = skip_json_ws(value, index + 1);
    let digits_start = index;
    while value.get(index).is_some_and(u8::is_ascii_digit) {
        index += 1;
    }
    if digits_start == index {
        return None;
    }
    std::str::from_utf8(&value[digits_start..index])
        .ok()?
        .parse()
        .ok()
}

fn skip_json_ws(value: &[u8], mut index: usize) -> usize {
    while value
        .get(index)
        .is_some_and(|byte| matches!(byte, b' ' | b'\n' | b'\r' | b'\t'))
    {
        index += 1;
    }
    index
}

fn retention_default_for_cf(cf_name: &str) -> StorageResult<RetentionDefault> {
    DEFAULTS
        .iter()
        .copied()
        .find(|default| default.cf == cf_name)
        .ok_or_else(|| {
            migration_read_failed(
                cf_name,
                &format!("missing retention default for column family {cf_name}"),
            )
        })
}

fn ttl_nanos_for_policy(cf_name: &str, ttl: RetentionTtl) -> StorageResult<Option<u64>> {
    match ttl {
        RetentionTtl::None | RetentionTtl::LruOnly => Ok(None),
        RetentionTtl::Hours(hours) => {
            if hours == 0 {
                return Err(migration_read_failed(
                    cf_name,
                    "RetentionDefault Hours(0) is invalid for migration TTL",
                ));
            }
            hours
                .checked_mul(SECONDS_PER_HOUR)
                .and_then(|seconds| seconds.checked_mul(NANOS_PER_SECOND))
                .map(Some)
                .ok_or_else(|| {
                    migration_read_failed(
                        cf_name,
                        &format!("RetentionDefault Hours({hours}) overflows nanoseconds"),
                    )
                })
        }
        RetentionTtl::Days(days) => {
            if days == 0 {
                return Err(migration_read_failed(
                    cf_name,
                    "RetentionDefault Days(0) is invalid for migration TTL",
                ));
            }
            days.checked_mul(HOURS_PER_DAY)
                .and_then(|hours| hours.checked_mul(SECONDS_PER_HOUR))
                .and_then(|seconds| seconds.checked_mul(NANOS_PER_SECOND))
                .map(Some)
                .ok_or_else(|| {
                    migration_read_failed(
                        cf_name,
                        &format!("RetentionDefault Days({days}) overflows nanoseconds"),
                    )
                })
        }
    }
}

fn ttl_policy_string(retention: RetentionDefault) -> String {
    match retention.ttl {
        RetentionTtl::None => "none".to_owned(),
        RetentionTtl::LruOnly => "lru_only".to_owned(),
        RetentionTtl::Hours(hours) => format!("hours:{hours}"),
        RetentionTtl::Days(days) => format!("days:{days}"),
    }
}

fn row_part_sha(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("sha256:{}", hex_lower(&hasher.finalize()))
}

fn resolved_retired_rocksdb_path(config: &StorageMigrationConfig) -> PathBuf {
    config.retired_rocksdb_path.clone().unwrap_or_else(|| {
        config.source_rocksdb_path.with_file_name(format!(
            "{}.rocksdb-retired",
            config
                .source_rocksdb_path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("db")
        ))
    })
}

fn stage_manifest(
    path: &Path,
    manifest: &StorageMigrationManifest,
) -> StorageResult<StagedManifest> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|source| StorageError::WriteFailed {
            cf_name: "<migration-manifest>".to_owned(),
            detail: format!("create manifest parent {}: {source}", parent.display()),
        })?;
    }
    let bytes = manifest_bytes(manifest)?;
    let tmp_path = manifest_staging_path(path);
    {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp_path)
            .map_err(|source| StorageError::WriteFailed {
                cf_name: "<migration-manifest>".to_owned(),
                detail: format!("create manifest staging {}: {source}", tmp_path.display()),
            })?;
        file.write_all(&bytes)
            .and_then(|()| file.sync_all())
            .map_err(|source| StorageError::WriteFailed {
                cf_name: "<migration-manifest>".to_owned(),
                detail: format!("write manifest staging {}: {source}", tmp_path.display()),
            })?;
    }
    let staged_readback = fs::read(&tmp_path).map_err(|source| StorageError::ReadFailed {
        cf_name: "<migration-manifest>".to_owned(),
        detail: format!("read manifest staging {}: {source}", tmp_path.display()),
    })?;
    if staged_readback != bytes {
        return Err(StorageError::ReadFailed {
            cf_name: "<migration-manifest>".to_owned(),
            detail: format!(
                "manifest staging readback differs at {}: expected_sha256={} actual_sha256={}",
                tmp_path.display(),
                row_part_sha(&bytes),
                row_part_sha(&staged_readback)
            ),
        });
    }
    let sha256 = row_part_sha(&bytes);
    tracing::info!(
        code = "STORAGE_MIGRATION_MANIFEST_STAGED",
        manifest_staging_path = %tmp_path.display(),
        final_manifest_path = %path.display(),
        manifest_bytes = bytes.len(),
        manifest_sha256 = %sha256,
        total_rows = manifest.total_rows,
        source_digest_sha256 = %manifest.source_digest_sha256,
        target_digest_sha256 = %manifest.target_digest_sha256,
        "staged durable storage migration manifest before source retirement"
    );
    Ok(StagedManifest {
        path: tmp_path,
        bytes,
        sha256,
    })
}

fn commit_manifest(
    path: &Path,
    manifest: &StorageMigrationManifest,
    staged: &StagedManifest,
) -> StorageResult<()> {
    replace_manifest_file(&staged.path, path).map_err(|source| StorageError::WriteFailed {
        cf_name: "<migration-manifest>".to_owned(),
        detail: format!(
            "commit manifest staging {} to {}: {source}",
            staged.path.display(),
            path.display()
        ),
    })?;
    let persisted = fs::read(path).map_err(|source| StorageError::ReadFailed {
        cf_name: "<migration-manifest>".to_owned(),
        detail: format!("read committed manifest {}: {source}", path.display()),
    })?;
    if persisted != staged.bytes {
        return Err(StorageError::ReadFailed {
            cf_name: "<migration-manifest>".to_owned(),
            detail: format!(
                "committed manifest readback differs at {}: expected_sha256={} actual_sha256={}",
                path.display(),
                staged.sha256,
                row_part_sha(&persisted)
            ),
        });
    }
    let decoded: StorageMigrationManifest =
        serde_json::from_slice(&persisted).map_err(|source| StorageError::DecodeJson {
            type_name: "StorageMigrationManifest",
            source,
        })?;
    if decoded != *manifest {
        return Err(StorageError::ReadFailed {
            cf_name: "<migration-manifest>".to_owned(),
            detail: format!(
                "committed manifest structured readback differs at {}",
                path.display()
            ),
        });
    }
    tracing::info!(
        code = "STORAGE_MIGRATION_MANIFEST_WRITTEN",
        manifest_path = %path.display(),
        manifest_bytes = persisted.len(),
        manifest_sha256 = %staged.sha256,
        total_rows = manifest.total_rows,
        source_digest_sha256 = %manifest.source_digest_sha256,
        target_digest_sha256 = %manifest.target_digest_sha256,
        "wrote durable storage migration manifest"
    );
    Ok(())
}

fn manifest_bytes(manifest: &StorageMigrationManifest) -> StorageResult<Vec<u8>> {
    let mut bytes =
        serde_json::to_vec_pretty(manifest).map_err(|source| StorageError::EncodeJson {
            type_name: "StorageMigrationManifest",
            source,
        })?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn manifest_staging_path(path: &Path) -> PathBuf {
    let mut file_name = path.file_name().map_or_else(
        || OsString::from("storage-migration-manifest.json"),
        OsString::from,
    );
    file_name.push(format!(".tmp.{}.{}", process::id(), now_unix_ms()));
    path.with_file_name(file_name)
}

fn replace_manifest_file(source: &Path, destination: &Path) -> io::Result<()> {
    fs::rename(source, destination)
}

fn rename_verified_source(source: &PathBuf, retired: &PathBuf) -> StorageResult<()> {
    if retired.exists() {
        return Err(StorageError::WriteFailed {
            cf_name: "<migration-rename>".to_owned(),
            detail: format!("retired RocksDB path already exists: {}", retired.display()),
        });
    }
    fs::rename(source, retired).map_err(|source_error| StorageError::WriteFailed {
        cf_name: "<migration-rename>".to_owned(),
        detail: format!(
            "rename verified RocksDB source {} to {}: {source_error}",
            source.display(),
            retired.display()
        ),
    })?;
    tracing::warn!(
        code = "STORAGE_MIGRATION_SOURCE_RETIRED",
        source_rocksdb_path = %source.display(),
        retired_rocksdb_path = %retired.display(),
        "renamed verified RocksDB source after successful migration"
    );
    Ok(())
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        })
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len().saturating_mul(2));
    for byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

fn migration_read_failed(cf_name: &str, detail: &str) -> StorageError {
    StorageError::ReadFailed {
        cf_name: cf_name.to_owned(),
        detail: detail.to_owned(),
    }
}
