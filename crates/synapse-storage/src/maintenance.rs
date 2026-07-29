//! Off-runtime admission for long-running storage maintenance.
//!
//! Storage GC, disk-pressure compaction, and Calyx native-CF / tombstone-purge
//! passes are synchronous, CPU- and I/O-heavy, and can run for minutes over
//! hundreds of megabytes. Running them inline on a Tokio runtime worker (the
//! prior behaviour of the periodic GC/pressure loops) parked that worker for the
//! whole pass and starved every MCP request sharing the runtime — an
//! `initialize` handshake and even pre-storage typed-param validation could time
//! out because no worker was free to poll them (issue #1798).
//!
//! This module routes every heavy maintenance pass through
//! [`tokio::task::spawn_blocking`], which runs it on Tokio's dedicated blocking
//! thread pool instead of a runtime worker, guarded by a small dedicated
//! semaphore so overlapping periodic ticks queue instead of piling onto the
//! blocking pool. This mirrors how mature LSM engines isolate background
//! compaction onto a dedicated, lower-priority thread pool so foreground request
//! latency is unaffected.

use std::path::{Path, PathBuf};
use std::sync::{
    Arc, LazyLock, Mutex, Weak,
    atomic::{AtomicU64, Ordering},
};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use synapse_calyx::{
    LOWERED_ARTIFACT_MAGIC, LOWERED_ARTIFACT_SCHEMA_VERSION, LOWERED_DIR_NAME,
    LoweredArtifactEnvelope, LoweredArtifactHandle, LoweredArtifactKind, LoweredFingerprint,
    LoweredGuardThresholds, SynapseCalyxClockMode, SynapseCalyxTuningConfig,
    SynapseCalyxVaultStatus, hot_context,
};
use tokio::sync::Semaphore;

use crate::{Db, StorageError, StorageResult};

/// Maximum heavy storage-maintenance passes admitted concurrently onto the
/// blocking pool. Kept small on purpose: native-CF compaction and tombstone
/// purge already serialize cross-process on Calyx's native-compaction file lock,
/// so this only needs to keep the periodic GC and disk-pressure loops from
/// stacking long passes onto the blocking pool at once while still letting an
/// urgent pressure pass proceed alongside a routine GC pass.
const MAX_CONCURRENT_STORAGE_MAINTENANCE_OPERATIONS: usize = 2;

static STORAGE_MAINTENANCE_PERMITS: LazyLock<Arc<Semaphore>> = LazyLock::new(|| {
    Arc::new(Semaphore::new(
        MAX_CONCURRENT_STORAGE_MAINTENANCE_OPERATIONS,
    ))
});

/// In-flight admitted maintenance passes, published as queue-depth telemetry.
static STORAGE_MAINTENANCE_IN_FLIGHT: AtomicU64 = AtomicU64::new(0);

/// Runs one blocking storage-maintenance pass off the async runtime workers.
///
/// The closure executes on Tokio's blocking pool under a dedicated admission
/// permit, so it can never park a runtime worker that is polling MCP requests.
/// Structured telemetry records the admission queue depth, the time spent
/// waiting for a permit, and the execution time of the pass itself.
///
/// # Errors
///
/// Returns the closure's error, or a structured storage error if the admission
/// semaphore was closed or the blocking task failed to join.
pub async fn run_admitted_maintenance<T, F>(operation: &'static str, work: F) -> StorageResult<T>
where
    F: FnOnce() -> StorageResult<T> + Send + 'static,
    T: Send + 'static,
{
    let semaphore = Arc::clone(&STORAGE_MAINTENANCE_PERMITS);
    let waiters_before =
        MAX_CONCURRENT_STORAGE_MAINTENANCE_OPERATIONS.saturating_sub(semaphore.available_permits());
    let admission_started = Instant::now();
    let permit = semaphore
        .acquire_owned()
        .await
        .map_err(|_closed| StorageError::WriteFailed {
            cf_name: "storage_maintenance".to_owned(),
            detail: format!(
                "{operation}: storage maintenance admission semaphore was unexpectedly closed"
            ),
        })?;
    let admission_wait_ms =
        u64::try_from(admission_started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let in_flight = STORAGE_MAINTENANCE_IN_FLIGHT
        .fetch_add(1, Ordering::AcqRel)
        .saturating_add(1);
    tracing::info!(
        code = "STORAGE_MAINTENANCE_ADMITTED",
        operation,
        admission_wait_ms,
        already_running = waiters_before as u64,
        in_flight,
        max_concurrent = MAX_CONCURRENT_STORAGE_MAINTENANCE_OPERATIONS as u64,
        "admitted storage maintenance onto the dedicated blocking pool off the async runtime workers"
    );
    let exec_started = Instant::now();
    let joined = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let outcome = work();
        // Hot-path boundary (#1686). Lowering the guard-threshold hot set is
        // off-runtime work by construction, so it rides the same admitted
        // blocking pass rather than acquiring a cadence of its own. It runs
        // after `work()` so the artifact it freezes reflects the state that
        // pass just produced, and its own failures never mask the maintenance
        // result.
        publish_lowered_guard_thresholds();
        outcome
    })
    .await;
    let exec_ms = u64::try_from(exec_started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let remaining = STORAGE_MAINTENANCE_IN_FLIGHT
        .fetch_sub(1, Ordering::AcqRel)
        .saturating_sub(1);
    match joined {
        Ok(result) => {
            tracing::info!(
                code = "STORAGE_MAINTENANCE_COMPLETED",
                operation,
                exec_ms,
                admission_wait_ms,
                in_flight = remaining,
                is_ok = result.is_ok(),
                "completed off-runtime storage maintenance pass"
            );
            result
        }
        Err(join_error) => Err(StorageError::WriteFailed {
            cf_name: "storage_maintenance".to_owned(),
            detail: format!(
                "{operation}: storage maintenance blocking task failed to join: {join_error}"
            ),
        }),
    }
}

// ---------------------------------------------------------------------------
// Off-tick lowering of the guard-threshold hot set (#1686)
// ---------------------------------------------------------------------------
//
// Doctrine (epic #1684): the reflex tick may not issue a live Calyx call, so
// every piece of Calyx-derived intelligence it consumes must be *lowered* --
// computed off-runtime, frozen into a content-fingerprinted artifact, published
// atomically, and read through a pointer swap. This is the producer half. It
// deliberately lives on the admitted maintenance pass, which already runs on
// Tokio's blocking pool under a dedicated permit, so publishing can never park
// a runtime worker and can never be reached from a tick.
//
// Scope is `GuardThresholds` only. The armed-routine constellation,
// next-occurrence window and kernel hot-set families named in the epic depend on
// #1677/#1678, and `calyx-ward` / `calyx-oracle` are not dependencies of any
// Synapse crate yet, so there is nothing real to lower for them; inventing
// artifact kinds for them now would publish empty files that a hot path could
// mistake for intelligence.
//
// The published bytes are verified by re-reading them through the *consumer*
// (`LoweredArtifactHandle::refresh`) before the publish is counted a success.
// That makes the reader -- the thing that actually has to trust the file -- the
// arbiter of a correct publish, rather than the writer grading its own work.

/// Reader-side wall-clock staleness bound stamped into published artifacts.
///
/// Comfortably longer than the maintenance cadence so an ordinary scheduling
/// delay does not flip a healthy hot path onto the fail-closed defaults, while
/// still bounding how long a vault that stopped running maintenance can keep a
/// tick on frozen values.
pub const LOWERED_GUARD_THRESHOLDS_STALENESS_BOUND_MS: u64 = 30 * 60 * 1000;

/// Publish attempts made from the maintenance pass.
static LOWERING_PUBLISH_ATTEMPTS: AtomicU64 = AtomicU64::new(0);
/// Publishes that wrote an artifact and re-verified it through the consumer.
static LOWERING_PUBLISH_SUCCESS: AtomicU64 = AtomicU64::new(0);
/// Publishes that failed.
static LOWERING_PUBLISH_FAILURE: AtomicU64 = AtomicU64::new(0);
/// Publishes skipped because no open, vault-backed storage handle is registered.
static LOWERING_PUBLISH_SKIPPED: AtomicU64 = AtomicU64::new(0);
/// Monotonic artifact generation, advanced once per successful publish.
static LOWERING_GENERATION: AtomicU64 = AtomicU64::new(0);

/// Storage handle the lowering pass reads vault state from.
///
/// A `Weak` on purpose: the registry must not be the reason a closed vault's
/// handle stays alive. When the daemon drops storage, the next pass observes a
/// dead weak reference and reports the skip rather than resurrecting anything.
static LOWERING_SOURCE: LazyLock<Mutex<Option<Weak<Db>>>> = LazyLock::new(|| Mutex::new(None));

static LOWERING_LAST: LazyLock<Mutex<LoweringPublishReadback>> =
    LazyLock::new(|| Mutex::new(LoweringPublishReadback::default()));

/// Externally readable outcome of the lowering publisher.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct LoweringPublishReadback {
    pub attempts_total: u64,
    pub success_total: u64,
    pub failure_total: u64,
    pub skipped_total: u64,
    pub last_success_unix_ms: Option<u64>,
    pub last_content_sha256: Option<String>,
    pub last_path: Option<PathBuf>,
    pub last_generation: Option<u64>,
    pub last_source_ledger_seq: Option<u64>,
    pub last_error_code: Option<String>,
    pub last_error: Option<String>,
}

/// Registers the storage handle whose Calyx vault the lowering pass reads.
///
/// Called once by the daemon when it starts the storage maintenance tasks.
/// Re-registration replaces the previous handle, which is what a vault reopen
/// requires.
pub fn register_lowering_source(db: &Arc<Db>) {
    let weak = Arc::downgrade(db);
    match LOWERING_SOURCE.lock() {
        Ok(mut guard) => *guard = Some(weak),
        Err(poisoned) => *poisoned.into_inner() = Some(weak),
    }
    tracing::info!(
        code = "STORAGE_LOWERING_SOURCE_REGISTERED",
        db_path = %db.path.display(),
        "registered the storage handle the off-tick guard-threshold lowering pass reads"
    );
}

/// Current publisher counters and last outcome.
#[must_use]
pub fn lowering_publish_readback() -> LoweringPublishReadback {
    let mut readback = match LOWERING_LAST.lock() {
        Ok(guard) => guard.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    };
    readback.attempts_total = LOWERING_PUBLISH_ATTEMPTS.load(Ordering::Relaxed);
    readback.success_total = LOWERING_PUBLISH_SUCCESS.load(Ordering::Relaxed);
    readback.failure_total = LOWERING_PUBLISH_FAILURE.load(Ordering::Relaxed);
    readback.skipped_total = LOWERING_PUBLISH_SKIPPED.load(Ordering::Relaxed);
    readback
}

fn record_lowering_skip(code: &'static str, detail: String) {
    LOWERING_PUBLISH_SKIPPED.fetch_add(1, Ordering::Relaxed);
    let mut guard = match LOWERING_LAST.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    guard.last_error_code = Some(code.to_owned());
    guard.last_error = Some(detail);
}

fn record_lowering_failure(code: &'static str, detail: &str) {
    LOWERING_PUBLISH_FAILURE.fetch_add(1, Ordering::Relaxed);
    tracing::error!(
        code,
        detail,
        "off-tick guard-threshold lowering pass failed; the reflex hot path stays on its \
         documented fail-closed defaults until a publish succeeds"
    );
    let mut guard = match LOWERING_LAST.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    guard.last_error_code = Some(code.to_owned());
    guard.last_error = Some(detail.to_owned());
}

/// Runs one lowering publish, if a vault-backed source is registered.
///
/// Never returns an error: a lowering failure must not turn a successful GC,
/// checkpoint or pressure pass into a failed one. It is recorded in the
/// counters and the structured log instead, and `health` surfaces both.
pub(crate) fn publish_lowered_guard_thresholds() {
    hot_context::assert_cold_calyx("maintenance_lower_guard_thresholds");
    LOWERING_PUBLISH_ATTEMPTS.fetch_add(1, Ordering::Relaxed);
    let source = match LOWERING_SOURCE.lock() {
        Ok(guard) => guard.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    };
    let Some(db) = source.as_ref().and_then(Weak::upgrade) else {
        record_lowering_skip(
            "STORAGE_LOWERING_SOURCE_UNREGISTERED",
            "no live storage handle is registered for guard-threshold lowering; call \
             synapse_storage::maintenance::register_lowering_source when storage opens"
                .to_owned(),
        );
        return;
    };
    let status = match db.calyx_vault_status() {
        Ok(status) => status,
        Err(error) => {
            record_lowering_failure(
                "STORAGE_LOWERING_VAULT_STATUS_FAILED",
                &format!("read Calyx vault status for guard-threshold lowering: {error}"),
            );
            return;
        }
    };
    if let Err((code, detail)) = publish_from_status(&status) {
        record_lowering_failure(code, &detail);
    }
}

fn publish_from_status(status: &SynapseCalyxVaultStatus) -> Result<(), (&'static str, String)> {
    if !status.open {
        record_lowering_skip(
            "STORAGE_LOWERING_VAULT_NOT_OPEN",
            format!(
                "Calyx vault is not open (enabled={} phase={}); nothing to lower",
                status.enabled, status.phase
            ),
        );
        return Ok(());
    }
    let (Some(vault_dir), Some(tuning), Some(vault_id)) = (
        status.vault_dir.as_ref(),
        status.tuning.as_ref(),
        status.vault_id.as_ref(),
    ) else {
        return Err((
            "STORAGE_LOWERING_VAULT_STATE_INCOMPLETE",
            format!(
                "open Calyx vault reported an incomplete state for lowering (vault_dir={} \
                 tuning={} vault_id={})",
                status.vault_dir.is_some(),
                status.tuning.is_some(),
                status.vault_id.is_some()
            ),
        ));
    };
    let envelope = freeze_guard_threshold_envelope(status, tuning, vault_id)?;
    let content_sha256 = envelope.fingerprint.content_sha256.clone();
    let produced_at_unix_ms = envelope.fingerprint.produced_at_unix_ms;
    let generation = envelope.fingerprint.generation;
    let bytes = serde_json::to_vec_pretty(&envelope).map_err(|error| {
        (
            "STORAGE_LOWERING_ENCODE_FAILED",
            format!("encode lowered guard-threshold envelope: {error}"),
        )
    })?;
    let dir = vault_dir.join(LOWERED_DIR_NAME);
    let path = dir.join(LoweredArtifactKind::GuardThresholds.file_name());
    atomic_publish(&dir, &path, &bytes)?;
    verify_published_artifact(vault_dir, &path, produced_at_unix_ms)?;

    LOWERING_PUBLISH_SUCCESS.fetch_add(1, Ordering::Relaxed);
    {
        let mut guard = match LOWERING_LAST.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.last_success_unix_ms = Some(produced_at_unix_ms);
        guard.last_content_sha256 = Some(content_sha256.clone());
        guard.last_path = Some(path.clone());
        guard.last_generation = Some(generation);
        guard.last_source_ledger_seq = Some(envelope.fingerprint.source_ledger_seq);
        guard.last_error_code = None;
        guard.last_error = None;
    }
    tracing::info!(
        code = "STORAGE_LOWERED_ARTIFACT_PUBLISHED",
        kind = %LoweredArtifactKind::GuardThresholds,
        path = %path.display(),
        content_sha256 = %content_sha256,
        generation,
        source_ledger_seq = envelope.fingerprint.source_ledger_seq,
        bytes_len = bytes.len(),
        "published and re-verified the lowered guard-threshold artifact off-tick"
    );
    Ok(())
}

/// Freezes the vault's current guard-threshold hot set into a fingerprinted
/// envelope.
///
/// The fingerprint is a SHA-256 over the canonical payload bytes, produced with
/// the same encoder the reader uses to re-derive them. `produced_at_unix_ms`
/// comes from the vault's own clock mode, not from wall-clock time, so a vault
/// pinned to a fixed clock does not get an artifact stamped with a time it does
/// not believe in.
fn freeze_guard_threshold_envelope(
    status: &SynapseCalyxVaultStatus,
    tuning: &SynapseCalyxTuningConfig,
    vault_id: &str,
) -> Result<LoweredArtifactEnvelope, (&'static str, String)> {
    let payload = LoweredGuardThresholds {
        bit_floor_bits: tuning.bit_floor_bits,
        correlation_ceiling: tuning.correlation_ceiling,
        guard_far_identity: tuning.guard_far_identity,
        guard_far_content: tuning.guard_far_content,
        guard_far_stylistic: tuning.guard_far_stylistic,
        guard_cold_start_tau: tuning.guard_cold_start_tau,
        kernel_fraction: tuning.kernel_fraction,
        kernel_recall_gate: tuning.kernel_recall_gate,
    };
    let canonical = serde_json::to_vec(&payload).map_err(|error| {
        (
            "STORAGE_LOWERING_ENCODE_FAILED",
            format!("encode frozen guard-threshold payload: {error}"),
        )
    })?;
    let produced_at_unix_ms = match tuning.clock_mode {
        SynapseCalyxClockMode::System => unix_time_ms_now(),
        SynapseCalyxClockMode::Fixed => tuning.fixed_clock_unix_ms.ok_or_else(|| {
            (
                "STORAGE_LOWERING_CLOCK_INVALID",
                "clock_mode=fixed is missing fixed_clock_unix_ms; refusing to stamp a lowered \
                 artifact with a clock the vault does not have"
                    .to_owned(),
            )
        })?,
    };
    Ok(LoweredArtifactEnvelope {
        magic: LOWERED_ARTIFACT_MAGIC.to_owned(),
        kind: LoweredArtifactKind::GuardThresholds,
        fingerprint: LoweredFingerprint {
            schema_version: LOWERED_ARTIFACT_SCHEMA_VERSION,
            content_sha256: sha256_hex(&canonical),
            generation: LOWERING_GENERATION
                .fetch_add(1, Ordering::Relaxed)
                .saturating_add(1),
            producing_panel_versions: Vec::new(),
            producing_lens_ids: Vec::new(),
            source_ledger_seq: status.latest_seq.unwrap_or_default(),
            produced_at_unix_ms,
            vault_id: vault_id.to_owned(),
        },
        staleness_bound_ms: LOWERED_GUARD_THRESHOLDS_STALENESS_BOUND_MS,
        payload,
    })
}

/// Proves the just-written bytes through the consumer that will actually read
/// them.
///
/// `refresh` re-parses the envelope, re-checks magic/kind/schema, recomputes the
/// SHA-256 over the frozen payload and compares it against the recorded
/// fingerprint. Anything short of `Fresh` means the file on disk is not
/// something a hot path may trust, so the publish is a failure even though the
/// write itself returned success.
fn verify_published_artifact(
    vault_dir: &Path,
    path: &Path,
    now_unix_ms: u64,
) -> Result<(), (&'static str, String)> {
    let verifier = LoweredArtifactHandle::unloaded(vault_dir, LoweredArtifactKind::GuardThresholds);
    let outcome = verifier.refresh(now_unix_ms);
    if outcome.became_fresh {
        return Ok(());
    }
    let reason = outcome.safe_default.map_or_else(
        || "unknown".to_owned(),
        |default| format!("{}: {}", default.code, default.message),
    );
    Err((
        "STORAGE_LOWERING_VERIFY_FAILED",
        format!(
            "published lowered guard-threshold artifact at {} did not read back as fresh through \
             the artifact consumer: {reason}",
            path.display()
        ),
    ))
}

/// Writes `bytes` to `path` atomically: sibling temp file, `sync_all`, rename.
/// A concurrent reader sees either the previous artifact or the new one, never a
/// partial write.
fn atomic_publish(dir: &Path, path: &Path, bytes: &[u8]) -> Result<(), (&'static str, String)> {
    use std::io::Write as _;

    std::fs::create_dir_all(dir).map_err(|error| {
        (
            "STORAGE_LOWERING_DIR_FAILED",
            format!(
                "create lowered artifact directory {}: {error}",
                dir.display()
            ),
        )
    })?;
    let tmp_path = path.with_extension("json.tmp");
    let mut file = std::fs::File::create(&tmp_path).map_err(|error| {
        (
            "STORAGE_LOWERING_WRITE_FAILED",
            format!(
                "create lowered artifact temp file {}: {error}",
                tmp_path.display()
            ),
        )
    })?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|error| {
            (
                "STORAGE_LOWERING_WRITE_FAILED",
                format!(
                    "write and flush lowered artifact temp file {}: {error}",
                    tmp_path.display()
                ),
            )
        })?;
    drop(file);
    std::fs::rename(&tmp_path, path).map_err(|error| {
        let _ = std::fs::remove_file(&tmp_path);
        (
            "STORAGE_LOWERING_PUBLISH_FAILED",
            format!(
                "atomically rename lowered artifact into {}: {error}",
                path.display()
            ),
        )
    })?;
    Ok(())
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest as _, Sha256};

    const HEX: &[u8; 16] = b"0123456789abcdef";
    let digest = Sha256::digest(bytes);
    let mut encoded = String::with_capacity(digest.len() * 2);
    for byte in digest {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}

fn unix_time_ms_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| u64::try_from(elapsed.as_millis()).ok())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use synapse_calyx::{
        LOWERED_DIR_NAME as DIR, LoweredArtifactState, SynapseCalyxTuningConfig,
        SynapseCalyxVaultStatus,
    };

    fn scratch_vault_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "synapse-lowering-{name}-{}-{}",
            std::process::id(),
            unix_time_ms_now()
        ));
        std::fs::create_dir_all(&dir).expect("create scratch vault dir");
        dir
    }

    fn open_status(vault_dir: &Path) -> SynapseCalyxVaultStatus {
        SynapseCalyxVaultStatus {
            enabled: true,
            phase: "open".to_owned(),
            open: true,
            vault_dir: Some(vault_dir.to_path_buf()),
            vault_id: Some("test-vault".to_owned()),
            latest_seq: Some(1234),
            tuning: Some(SynapseCalyxTuningConfig::default()),
            ..SynapseCalyxVaultStatus::default()
        }
    }

    /// The producer's fingerprint must be exactly what the consumer recomputes.
    ///
    /// This is the contract the whole hot-path boundary rests on: the tick trusts
    /// a frozen file only because the reader re-derives its content hash. If the
    /// two encoders ever drift the artifact degrades to the fail-closed default
    /// and the tick silently stops consuming lowered intelligence, so the round
    /// trip is asserted here rather than assumed.
    #[test]
    fn published_artifact_round_trips_through_the_consumer() {
        let vault_dir = scratch_vault_dir("roundtrip");
        let status = open_status(&vault_dir);

        publish_from_status(&status).expect("publish lowered guard thresholds");

        let path = vault_dir
            .join(DIR)
            .join(LoweredArtifactKind::GuardThresholds.file_name());
        assert!(path.is_file(), "artifact was not published at {path:?}");

        let handle =
            LoweredArtifactHandle::unloaded(&vault_dir, LoweredArtifactKind::GuardThresholds);
        let outcome = handle.refresh(unix_time_ms_now());
        assert!(
            outcome.became_fresh,
            "consumer refused the published artifact: {:?}",
            outcome.safe_default
        );

        let state = handle.load();
        let LoweredArtifactState::Fresh(artifact) = state.as_ref() else {
            panic!("expected a fresh artifact, got {state:?}");
        };
        let tuning = SynapseCalyxTuningConfig::default();
        // Bit-exact, not approximately equal: the frozen payload must survive the
        // JSON round trip unchanged or its content hash would not match.
        assert_eq!(
            artifact.guard_thresholds.bit_floor_bits.to_bits(),
            tuning.bit_floor_bits.to_bits()
        );
        assert_eq!(artifact.fingerprint.source_ledger_seq, 1234);
        assert_eq!(artifact.fingerprint.vault_id, "test-vault");
        // The fingerprint must be the hash of the canonical payload bytes, not
        // of the envelope, so an operator comparing the two knows which is which.
        let canonical =
            serde_json::to_vec(&artifact.guard_thresholds).expect("re-encode frozen payload");
        assert_eq!(artifact.fingerprint.content_sha256, sha256_hex(&canonical));

        std::fs::remove_dir_all(&vault_dir).expect("clean scratch vault dir");
    }

    /// A closed vault must be reported as a skip, never published from.
    #[test]
    fn a_closed_vault_publishes_nothing() {
        let vault_dir = scratch_vault_dir("closed");
        let status = SynapseCalyxVaultStatus {
            open: false,
            vault_dir: Some(vault_dir.clone()),
            ..open_status(&vault_dir)
        };
        let before = LOWERING_PUBLISH_SKIPPED.load(Ordering::Relaxed);

        publish_from_status(&status).expect("a closed vault is a skip, not an error");

        assert_eq!(LOWERING_PUBLISH_SKIPPED.load(Ordering::Relaxed), before + 1);
        assert!(!vault_dir.join(DIR).exists());
        std::fs::remove_dir_all(&vault_dir).expect("clean scratch vault dir");
    }
}
