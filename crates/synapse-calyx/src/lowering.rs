//! Hot-path boundary: lower Calyx intelligence to frozen, fingerprinted
//! artifacts (#1686).
//!
//! Doctrine (epic #1684, `INTEGRATION_PLAN.md` §4.1): latency-critical
//! deterministic loops — the reflex tick, the capture frame path, and the
//! armed-routine matcher — must never issue a live Calyx call. Any intelligence
//! they consume is **lowered**: computed off-runtime, frozen into a
//! content-fingerprinted artifact on disk, atomically published, and read by the
//! hot path through a lock-free pointer swap. A missing, corrupt, or stale
//! artifact degrades to a documented, logged safe default — never a live Calyx
//! call from inside a tick.
//!
//! This module owns three things:
//! 1. The on-disk artifact format ([`LoweredArtifactEnvelope`]) and its
//!    fingerprint ([`LoweredFingerprint`]): content SHA-256 over the frozen
//!    payload plus the producing panel/lens versions and a monotonic
//!    generation. Any drift in the frozen intelligence changes the content hash
//!    and therefore the fingerprint.
//! 2. The producer, [`SynapseCalyxVault::lower_guard_thresholds`], which runs on
//!    the async storage/maintenance path (asserted cold — see [`hot_context`]),
//!    serializes the selected intelligence, and publishes it atomically
//!    (temp file, `sync_all`, atomic rename).
//! 3. The consumer, [`LoweredArtifactHandle`], a self-contained reader that
//!    keeps the parsed artifact behind an [`arc_swap::ArcSwap`]. The hot path
//!    calls [`LoweredArtifactHandle::load`] at tick start — one atomic load, no
//!    filesystem I/O, no Calyx. Refreshes ([`LoweredArtifactHandle::refresh`])
//!    happen off-tick and swap the pointer in place.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

use arc_swap::ArcSwap;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::{SynapseCalyxError, SynapseCalyxVault};

/// Directory, under the vault root, holding published lowered artifacts.
pub const LOWERED_DIR_NAME: &str = "lowered";
/// On-disk envelope schema version. Bumped only on an incompatible format
/// change; a reader that sees a different version fails closed.
pub const LOWERED_ARTIFACT_SCHEMA_VERSION: u32 = 1;
/// Magic string stamped into every envelope so a stray file cannot be mistaken
/// for a lowered artifact.
pub const LOWERED_ARTIFACT_MAGIC: &str = "SYN-LOWERED-V1";

// Documented safe defaults for the load-bearing guard FAR thresholds.
const DEFAULT_GUARD_FAR_IDENTITY: f32 = 0.01;
const DEFAULT_GUARD_FAR_CONTENT: f32 = 0.03;
const DEFAULT_GUARD_FAR_STYLISTIC: f32 = 0.05;

/// Thread-local hot-context boundary.
///
/// A latency-critical loop marks its worker thread with [`enter_hot_context`]
/// for the lifetime of the returned scope. Any Calyx-touching helper can then
/// call [`assert_cold_calyx`] to prove — loudly in debug builds, observably in
/// release — that it is not being driven from a tagged tick.
pub mod hot_context {
    use std::cell::Cell;

    thread_local! {
        static HOT_DEPTH: Cell<u32> = const { Cell::new(0) };
    }

    /// RAII guard that marks the current thread as a hot (latency-critical)
    /// context until it is dropped. Nesting is counted, so overlapping scopes on
    /// one thread compose correctly.
    #[derive(Debug)]
    pub struct HotContextScope {
        _private: (),
    }

    impl Drop for HotContextScope {
        fn drop(&mut self) {
            HOT_DEPTH.with(|depth| depth.set(depth.get().saturating_sub(1)));
        }
    }

    /// Marks the current thread as a hot context for the returned scope's
    /// lifetime.
    #[must_use]
    pub fn enter_hot_context() -> HotContextScope {
        HOT_DEPTH.with(|depth| depth.set(depth.get().saturating_add(1)));
        HotContextScope { _private: () }
    }

    /// Returns true when the current thread is inside a hot context.
    #[must_use]
    pub fn in_hot_context() -> bool {
        HOT_DEPTH.with(|depth| depth.get() > 0)
    }

    /// Enforces the hot-path boundary for one Calyx operation.
    ///
    /// In debug builds this panics if invoked from a tagged hot context — the
    /// acceptance-test assertion that proves zero live Calyx calls in a tick. In
    /// release builds it emits a structured error edge instead of aborting the
    /// daemon, so the violation is observable without taking the process down.
    pub fn assert_cold_calyx(operation: &'static str) {
        if !in_hot_context() {
            return;
        }
        tracing::error!(
            code = "SYNAPSE_CALYX_HOT_PATH_BOUNDARY_VIOLATION",
            operation,
            "a live Calyx operation was attempted from a hot (latency-critical) context; \
             hot paths must consume only lowered frozen artifacts"
        );
        debug_assert!(
            false,
            "SYNAPSE_CALYX_HOT_PATH_BOUNDARY_VIOLATION: live Calyx operation `{operation}` \
             attempted from a hot context; consume a lowered artifact instead"
        );
    }
}

/// The families of intelligence that can be lowered for hot-path consumption.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoweredArtifactKind {
    /// Ward guard thresholds + honesty-gate knobs consumed by reality/OOD
    /// checks adjacent to the reflex path.
    GuardThresholds,
}

impl LoweredArtifactKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::GuardThresholds => "guard_thresholds",
        }
    }

    /// The on-disk file name (under [`LOWERED_DIR_NAME`]) for this kind.
    #[must_use]
    pub fn file_name(self) -> String {
        format!("{}.artifact.json", self.as_str())
    }
}

impl fmt::Display for LoweredArtifactKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Frozen guard-threshold payload.
///
/// Every field is an explicit, reproducible value; there is no derived state and
/// no live handle. Serialized field order is fixed, so the canonical JSON bytes
/// — and therefore the content hash — are deterministic and change iff a
/// threshold changes (drift ⇒ new fingerprint).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoweredGuardThresholds {
    pub guard_far_identity: f32,
    pub guard_far_content: f32,
    pub guard_far_stylistic: f32,
}

impl LoweredGuardThresholds {
    /// The documented safe default consumed when no fresh artifact is available.
    ///
    /// These are the conservative daemon-startup defaults: the hot path fails
    /// closed to them rather than issuing a live Calyx call.
    #[must_use]
    pub const fn fail_closed_default() -> Self {
        Self {
            guard_far_identity: DEFAULT_GUARD_FAR_IDENTITY,
            guard_far_content: DEFAULT_GUARD_FAR_CONTENT,
            guard_far_stylistic: DEFAULT_GUARD_FAR_STYLISTIC,
        }
    }

    /// Canonical serialized bytes used for the content fingerprint.
    fn canonical_bytes(&self) -> Result<Vec<u8>, SynapseCalyxError> {
        serde_json::to_vec(self).map_err(|error| {
            SynapseCalyxError::new(
                "SYNAPSE_CALYX_LOWERING_ENCODE_FAILED",
                format!("encode lowered guard-threshold payload: {error}"),
                "inspect the guard-threshold payload shape; a frozen field failed to serialize",
            )
        })
    }
}

/// Content fingerprint proving what a lowered artifact was derived from.
///
/// `content_sha256` binds the exact frozen payload bytes. `generation`,
/// `producing_panel_versions`, and `producing_lens_ids` bind the producing
/// Calyx state so a consumer (or `reproduce`) can prove the artifact matches the
/// vault state it claims. Any drift in payload or producing versions yields a
/// different fingerprint.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoweredFingerprint {
    pub schema_version: u32,
    pub content_sha256: String,
    pub generation: u64,
    pub producing_panel_versions: Vec<(String, u32)>,
    pub producing_lens_ids: Vec<String>,
    pub source_ledger_seq: u64,
    pub produced_at_unix_ms: u64,
    pub vault_id: String,
}

/// The complete on-disk artifact: self-describing header, fingerprint,
/// reader-side staleness bound, and the frozen payload.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoweredArtifactEnvelope {
    pub magic: String,
    pub kind: LoweredArtifactKind,
    pub fingerprint: LoweredFingerprint,
    /// Maximum age, in milliseconds, a consumer should trust this artifact.
    /// `0` disables the wall-clock staleness bound (fingerprint drift still
    /// invalidates it).
    pub staleness_bound_ms: u64,
    pub payload: LoweredGuardThresholds,
}

/// Off-runtime inputs that pin a lowering pass to a producing Calyx state.
#[derive(Clone, Debug, Default)]
pub struct LoweringParams {
    /// Monotonic generation for this artifact family. Callers derive it from the
    /// producing pipeline (e.g. a panel generation allocation) so a stale swap
    /// is detectable.
    pub generation: u64,
    /// Producing panel `(name, version)` pairs feeding this intelligence.
    pub producing_panel_versions: Vec<(String, u32)>,
    /// Producing lens ids feeding this intelligence.
    pub producing_lens_ids: Vec<String>,
    /// Reader-side wall-clock staleness bound in milliseconds (`0` disables it).
    pub staleness_bound_ms: u64,
}

/// Result of one atomic publish.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct LoweredPublishReport {
    pub kind: LoweredArtifactKind,
    pub path: PathBuf,
    pub content_sha256: String,
    pub generation: u64,
    pub bytes_len: usize,
    pub produced_at_unix_ms: u64,
    pub source_ledger_seq: u64,
}

impl SynapseCalyxVault {
    /// Lowers the current guard-threshold hot set into a frozen, fingerprinted
    /// artifact published atomically under `<vault_dir>/lowered/`.
    ///
    /// **This is the only producer of the artifact (#1885).** The storage
    /// maintenance publisher used to rebuild the same payload, content
    /// fingerprint, clock stamp and atomic publish by hand, purely because the
    /// sole `Arc<SynapseCalyxVault>` sat behind a private `with_vault`; two
    /// independent producers of one on-disk format drift with nothing to catch
    /// it. That path is gone: maintenance now calls
    /// `Db::lower_guard_thresholds`, which delegates here. Anything that needs
    /// to publish this artifact must reach this function — do not reconstruct
    /// [`LoweredArtifactEnvelope`] anywhere else.
    ///
    /// Runs on the async storage/maintenance path and asserts a cold context
    /// (it must never be called from a tick). The payload is the exact, frozen
    /// tuning thresholds; the fingerprint binds the content hash plus the
    /// caller-supplied producing versions/generation. Publish is temp-file →
    /// `sync_all` → atomic rename, so a reader never observes a torn artifact.
    ///
    /// # Errors
    ///
    /// Returns a structured error when the payload cannot be encoded, the
    /// lowered directory cannot be created, or the atomic publish fails.
    pub fn lower_guard_thresholds(
        &self,
        params: &LoweringParams,
    ) -> Result<LoweredPublishReport, SynapseCalyxError> {
        hot_context::assert_cold_calyx("lower_guard_thresholds");
        let tuning = &self.config.tuning;
        let payload = LoweredGuardThresholds {
            guard_far_identity: tuning.guard_far_identity,
            guard_far_content: tuning.guard_far_content,
            guard_far_stylistic: tuning.guard_far_stylistic,
        };
        let content_sha256 = sha256_hex(&payload.canonical_bytes()?);
        let produced_at_unix_ms = self.clock_now_ms()?;
        let fingerprint = LoweredFingerprint {
            schema_version: LOWERED_ARTIFACT_SCHEMA_VERSION,
            content_sha256: content_sha256.clone(),
            generation: params.generation,
            producing_panel_versions: params.producing_panel_versions.clone(),
            producing_lens_ids: params.producing_lens_ids.clone(),
            source_ledger_seq: self.latest_seq(),
            produced_at_unix_ms,
            vault_id: self.vault_id(),
        };
        let envelope = LoweredArtifactEnvelope {
            magic: LOWERED_ARTIFACT_MAGIC.to_owned(),
            kind: LoweredArtifactKind::GuardThresholds,
            fingerprint,
            staleness_bound_ms: params.staleness_bound_ms,
            payload,
        };
        let bytes = serde_json::to_vec_pretty(&envelope).map_err(|error| {
            SynapseCalyxError::new(
                "SYNAPSE_CALYX_LOWERING_ENCODE_FAILED",
                format!("encode lowered artifact envelope: {error}"),
                "inspect the lowered artifact shape; the envelope failed to serialize",
            )
        })?;
        let dir = lowered_dir(&self.config.vault_dir);
        let path = dir.join(LoweredArtifactKind::GuardThresholds.file_name());
        atomic_publish(&dir, &path, &bytes)?;
        tracing::info!(
            code = "SYNAPSE_CALYX_LOWERED_ARTIFACT_PUBLISHED",
            kind = %LoweredArtifactKind::GuardThresholds,
            path = %path.display(),
            content_sha256 = %content_sha256,
            generation = params.generation,
            source_ledger_seq = envelope.fingerprint.source_ledger_seq,
            bytes_len = bytes.len(),
            "published lowered hot-path artifact"
        );
        Ok(LoweredPublishReport {
            kind: LoweredArtifactKind::GuardThresholds,
            path,
            content_sha256,
            generation: params.generation,
            bytes_len: bytes.len(),
            produced_at_unix_ms,
            source_ledger_seq: envelope.fingerprint.source_ledger_seq,
        })
    }
}

/// A parsed, fingerprint-verified artifact resident in memory.
#[derive(Clone, Debug, PartialEq)]
pub struct LoadedLoweredArtifact {
    pub kind: LoweredArtifactKind,
    pub fingerprint: LoweredFingerprint,
    pub staleness_bound_ms: u64,
    pub guard_thresholds: LoweredGuardThresholds,
    pub loaded_at_unix_ms: u64,
    pub bytes_len: usize,
}

impl LoadedLoweredArtifact {
    /// Age of the frozen artifact at `now_unix_ms`, in milliseconds.
    #[must_use]
    pub const fn age_ms(&self, now_unix_ms: u64) -> u64 {
        now_unix_ms.saturating_sub(self.fingerprint.produced_at_unix_ms)
    }

    /// Whether the artifact has exceeded its wall-clock staleness bound. A bound
    /// of `0` disables the check (fingerprint drift still invalidates it at
    /// refresh time).
    #[must_use]
    pub const fn is_stale(&self, now_unix_ms: u64) -> bool {
        self.staleness_bound_ms > 0 && self.age_ms(now_unix_ms) > self.staleness_bound_ms
    }
}

/// The documented safe-default state: fail-closed `{code, message, remediation}`
/// with no live artifact. The hot path degrades to built-in defaults and never
/// calls Calyx.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct LoweredSafeDefault {
    pub code: &'static str,
    pub message: String,
    pub remediation: &'static str,
}

impl LoweredSafeDefault {
    fn new(code: &'static str, message: impl Into<String>, remediation: &'static str) -> Self {
        Self {
            code,
            message: message.into(),
            remediation,
        }
    }
}

/// What a consumer sees at tick start: either a fresh, fingerprint-verified
/// artifact or the documented safe default.
#[derive(Clone, Debug, PartialEq)]
pub enum LoweredArtifactState {
    Fresh(LoadedLoweredArtifact),
    SafeDefault(LoweredSafeDefault),
}

impl LoweredArtifactState {
    /// Returns the guard thresholds to use at `now_unix_ms`. A fresh, non-stale
    /// artifact yields its frozen thresholds; anything else yields the
    /// fail-closed default. Never performs I/O or a Calyx call.
    #[must_use]
    pub const fn guard_thresholds(&self, now_unix_ms: u64) -> LoweredGuardThresholds {
        match self {
            Self::Fresh(artifact) if !artifact.is_stale(now_unix_ms) => artifact.guard_thresholds,
            Self::Fresh(_) | Self::SafeDefault(_) => LoweredGuardThresholds::fail_closed_default(),
        }
    }

    #[must_use]
    pub const fn is_fresh(&self) -> bool {
        matches!(self, Self::Fresh(_))
    }

    const fn transition_label(&self) -> &'static str {
        match self {
            Self::Fresh(_) => "fresh",
            Self::SafeDefault(_) => "safe_default",
        }
    }
}

/// Outcome of one off-tick refresh, for observability/counters.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoweredRefreshOutcome {
    pub kind: LoweredArtifactKind,
    pub became_fresh: bool,
    pub content_sha256: Option<String>,
    pub safe_default: Option<LoweredSafeDefault>,
    pub refresh_count: u64,
}

/// Self-contained reader that hot paths consume without touching Calyx.
///
/// The parsed state lives behind an [`ArcSwap`]. [`Self::load`] — the hot-path
/// call — is a single lock-free atomic load with no filesystem access.
/// [`Self::refresh`] re-reads and re-verifies the on-disk artifact off-tick and
/// swaps the pointer atomically.
#[derive(Debug)]
pub struct LoweredArtifactHandle {
    path: PathBuf,
    kind: LoweredArtifactKind,
    state: ArcSwap<LoweredArtifactState>,
    reads: AtomicU64,
    refreshes: AtomicU64,
}

impl LoweredArtifactHandle {
    /// Creates an unloaded handle for `kind` under `vault_dir`. It starts in the
    /// safe-default state; call [`Self::refresh`] off-tick to load the artifact.
    #[must_use]
    pub fn unloaded(vault_dir: &Path, kind: LoweredArtifactKind) -> Self {
        let path = lowered_dir(vault_dir).join(kind.file_name());
        let initial = LoweredArtifactState::SafeDefault(LoweredSafeDefault::new(
            "SYNAPSE_CALYX_LOWERED_ARTIFACT_UNLOADED",
            format!("lowered {kind} artifact has not been loaded yet"),
            "run a lowering refresh off-runtime before the hot path relies on this artifact",
        ));
        Self {
            path,
            kind,
            state: ArcSwap::from_pointee(initial),
            reads: AtomicU64::new(0),
            refreshes: AtomicU64::new(0),
        }
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub const fn kind(&self) -> LoweredArtifactKind {
        self.kind
    }

    /// Hot-path entry point. One lock-free atomic load; no I/O, no Calyx. Call
    /// this at tick start and read the returned state for the whole tick.
    #[must_use]
    pub fn load(&self) -> Arc<LoweredArtifactState> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        self.state.load_full()
    }

    /// Number of hot-path [`Self::load`] calls served (observability).
    #[must_use]
    pub fn read_count(&self) -> u64 {
        self.reads.load(Ordering::Relaxed)
    }

    /// Number of off-tick [`Self::refresh`] passes performed (observability).
    #[must_use]
    pub fn refresh_count(&self) -> u64 {
        self.refreshes.load(Ordering::Relaxed)
    }

    /// Re-reads and re-verifies the on-disk artifact, then atomically swaps the
    /// in-memory pointer. **Off-tick only.** A missing, corrupt, schema- or
    /// kind-mismatched, fingerprint-mismatched, or stale artifact swaps in a
    /// documented safe default and logs the transition — it never falls back to
    /// a live Calyx call. Returns an outcome for counters/observability.
    pub fn refresh(&self, now_unix_ms: u64) -> LoweredRefreshOutcome {
        let refresh_count = self.refreshes.fetch_add(1, Ordering::Relaxed) + 1;
        let next = self.evaluate(now_unix_ms);
        let previous = self.state.load();
        let changed = previous.transition_label() != next.transition_label()
            || content_hash_of(&previous) != content_hash_of(&next);
        if changed {
            log_transition(self.kind, &previous, &next);
        }
        let content_sha256 = content_hash_of(&next);
        let safe_default = match &next {
            LoweredArtifactState::SafeDefault(default) => Some(default.clone()),
            LoweredArtifactState::Fresh(_) => None,
        };
        let became_fresh = next.is_fresh();
        self.state.store(Arc::new(next));
        LoweredRefreshOutcome {
            kind: self.kind,
            became_fresh,
            content_sha256,
            safe_default,
            refresh_count,
        }
    }

    fn read_artifact_bytes(&self) -> Result<Vec<u8>, Box<LoweredArtifactState>> {
        match std::fs::read(&self.path) {
            Ok(bytes) => Ok(bytes),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Err(Box::new(safe_default(
                    "SYNAPSE_CALYX_LOWERED_ARTIFACT_MISSING",
                    format!(
                        "lowered {} artifact absent at {}",
                        self.kind,
                        self.path.display()
                    ),
                    "publish the lowered artifact off-runtime before the hot path relies on it",
                )))
            }
            Err(error) => Err(Box::new(safe_default(
                "SYNAPSE_CALYX_LOWERED_ARTIFACT_IO",
                format!(
                    "read lowered {} artifact {}: {error}",
                    self.kind,
                    self.path.display()
                ),
                "repair filesystem access to the lowered artifact directory and re-publish",
            ))),
        }
    }

    fn evaluate(&self, now_unix_ms: u64) -> LoweredArtifactState {
        let bytes = match self.read_artifact_bytes() {
            Ok(bytes) => bytes,
            Err(state) => return *state,
        };
        let bytes_len = bytes.len();
        let envelope: LoweredArtifactEnvelope = match serde_json::from_slice(&bytes) {
            Ok(envelope) => envelope,
            Err(error) => {
                return safe_default(
                    "SYNAPSE_CALYX_LOWERED_ARTIFACT_CORRUPT",
                    format!(
                        "decode lowered {} artifact {}: {error}",
                        self.kind,
                        self.path.display()
                    ),
                    "re-publish the lowered artifact from authoritative vault state",
                );
            }
        };
        if envelope.magic != LOWERED_ARTIFACT_MAGIC {
            return safe_default(
                "SYNAPSE_CALYX_LOWERED_ARTIFACT_MAGIC_INVALID",
                format!(
                    "lowered {} artifact has magic {:?}",
                    self.kind, envelope.magic
                ),
                "re-publish the lowered artifact; the file is not a valid lowered artifact",
            );
        }
        if envelope.kind != self.kind {
            return safe_default(
                "SYNAPSE_CALYX_LOWERED_ARTIFACT_KIND_MISMATCH",
                format!(
                    "lowered artifact at {} is kind {} but handle expects {}",
                    self.path.display(),
                    envelope.kind,
                    self.kind
                ),
                "re-publish the correct lowered artifact kind for this consumer",
            );
        }
        if envelope.fingerprint.schema_version != LOWERED_ARTIFACT_SCHEMA_VERSION {
            return safe_default(
                "SYNAPSE_CALYX_LOWERED_ARTIFACT_SCHEMA_UNSUPPORTED",
                format!(
                    "lowered {} artifact schema {} != supported {}",
                    self.kind, envelope.fingerprint.schema_version, LOWERED_ARTIFACT_SCHEMA_VERSION
                ),
                "re-publish the lowered artifact with the supported schema version",
            );
        }
        let canonical = match envelope.payload.canonical_bytes() {
            Ok(canonical) => canonical,
            Err(error) => {
                return safe_default(
                    "SYNAPSE_CALYX_LOWERED_ARTIFACT_CORRUPT",
                    format!(
                        "re-encode lowered {} payload for verification: {error}",
                        self.kind
                    ),
                    "re-publish the lowered artifact from authoritative vault state",
                );
            }
        };
        let recomputed = sha256_hex(&canonical);
        if recomputed != envelope.fingerprint.content_sha256 {
            return safe_default(
                "SYNAPSE_CALYX_LOWERED_ARTIFACT_FINGERPRINT_MISMATCH",
                format!(
                    "lowered {} artifact content hash {} != fingerprint {}",
                    self.kind, recomputed, envelope.fingerprint.content_sha256
                ),
                "re-publish the lowered artifact; its payload does not match its fingerprint",
            );
        }
        let loaded = LoadedLoweredArtifact {
            kind: envelope.kind,
            fingerprint: envelope.fingerprint,
            staleness_bound_ms: envelope.staleness_bound_ms,
            guard_thresholds: envelope.payload,
            loaded_at_unix_ms: now_unix_ms,
            bytes_len,
        };
        if loaded.is_stale(now_unix_ms) {
            return safe_default(
                "SYNAPSE_CALYX_LOWERED_ARTIFACT_STALE",
                format!(
                    "lowered {} artifact age {}ms exceeds staleness bound {}ms",
                    self.kind,
                    loaded.age_ms(now_unix_ms),
                    loaded.staleness_bound_ms
                ),
                "re-publish the lowered artifact off-runtime; the hot path is on the safe default",
            );
        }
        LoweredArtifactState::Fresh(loaded)
    }
}

fn safe_default(
    code: &'static str,
    message: impl Into<String>,
    remediation: &'static str,
) -> LoweredArtifactState {
    LoweredArtifactState::SafeDefault(LoweredSafeDefault::new(code, message, remediation))
}

fn content_hash_of(state: &LoweredArtifactState) -> Option<String> {
    match state {
        LoweredArtifactState::Fresh(artifact) => Some(artifact.fingerprint.content_sha256.clone()),
        LoweredArtifactState::SafeDefault(_) => None,
    }
}

fn log_transition(
    kind: LoweredArtifactKind,
    previous: &LoweredArtifactState,
    next: &LoweredArtifactState,
) {
    match next {
        LoweredArtifactState::Fresh(artifact) => {
            tracing::info!(
                code = "SYNAPSE_CALYX_LOWERED_ARTIFACT_FRESH",
                kind = %kind,
                from = previous.transition_label(),
                content_sha256 = %artifact.fingerprint.content_sha256,
                generation = artifact.fingerprint.generation,
                "lowered artifact refreshed to fresh state"
            );
        }
        LoweredArtifactState::SafeDefault(default) => {
            tracing::warn!(
                code = default.code,
                kind = %kind,
                from = previous.transition_label(),
                detail = %default.message,
                remediation = default.remediation,
                "lowered artifact degraded to safe default; hot path uses fail-closed defaults"
            );
        }
    }
}

fn lowered_dir(vault_dir: &Path) -> PathBuf {
    vault_dir.join(LOWERED_DIR_NAME)
}

/// Serialises publishes to one artifact path within this process.
///
/// Two publishers renaming over the same target is safe on its own; two
/// publishers sharing a temp *name* is not. This lock makes the write-then-
/// rename pair indivisible so the observed interleaving cannot recur even if a
/// future caller reintroduces a shared temp name.
static PUBLISH_LOCKS: LazyLock<Mutex<BTreeMap<PathBuf, Arc<Mutex<()>>>>> =
    LazyLock::new(|| Mutex::new(BTreeMap::new()));

/// Monotonic discriminator for temp-file names, so two publishes in the same
/// process and millisecond cannot collide.
static PUBLISH_TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn publish_lock_for(path: &Path) -> Arc<Mutex<()>> {
    let mut guard = match PUBLISH_LOCKS.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    Arc::clone(
        guard
            .entry(path.to_path_buf())
            .or_insert_with(|| Arc::new(Mutex::new(()))),
    )
}

/// Publishes `bytes` to `path` atomically: write a **uniquely named** sibling
/// temp file, flush it to disk, then rename it over the target. A reader either
/// sees the previous artifact or the new one, never a partial write.
///
/// The temp name carries the process id and a monotonic counter. A fixed
/// `<name>.json.tmp` was observed failing on the live daemon: two concurrent
/// publishers created the same temp path, the first renamed it away, and the
/// second's rename found nothing —
/// `STORAGE_LOWERING_PUBLISH_FAILED ... The system cannot find the file
/// specified. (os error 2)`, twice in the 30h before this fix, which is what
/// held `health.ok` at false. A shared temp path also means the loser can
/// publish the winner's bytes under its own generation stamp, so this is a
/// correctness bug and not only a spurious error.
fn atomic_publish(dir: &Path, path: &Path, bytes: &[u8]) -> Result<(), SynapseCalyxError> {
    use std::io::Write as _;

    std::fs::create_dir_all(dir).map_err(|error| {
        SynapseCalyxError::with_io(
            "SYNAPSE_CALYX_LOWERING_DIR_FAILED",
            "create lowered artifact directory",
            dir,
            &error,
            "repair filesystem access to the vault directory and retry the lowering pass",
        )
    })?;
    let path_lock = publish_lock_for(path);
    let _publishing = match path_lock.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    let tmp_path = path.with_extension(format!(
        "json.{}.{}.tmp",
        std::process::id(),
        PUBLISH_TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let mut file = std::fs::File::create(&tmp_path).map_err(|error| {
        SynapseCalyxError::with_io(
            "SYNAPSE_CALYX_LOWERING_WRITE_FAILED",
            "create lowered artifact temp file",
            &tmp_path,
            &error,
            "repair filesystem access to the lowered artifact directory and retry",
        )
    })?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|error| {
            SynapseCalyxError::with_io(
                "SYNAPSE_CALYX_LOWERING_WRITE_FAILED",
                "write and flush lowered artifact temp file",
                &tmp_path,
                &error,
                "repair filesystem access to the lowered artifact directory and retry",
            )
        })?;
    drop(file);
    std::fs::rename(&tmp_path, path).map_err(|error| {
        let _ = std::fs::remove_file(&tmp_path);
        SynapseCalyxError::with_io(
            "SYNAPSE_CALYX_LOWERING_PUBLISH_FAILED",
            "atomically rename lowered artifact into place",
            path,
            &error,
            "repair filesystem access to the lowered artifact directory and retry",
        )
    })?;
    Ok(())
}

fn sha256_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let digest = Sha256::digest(bytes);
    let mut encoded = String::with_capacity(digest.len() * 2);
    for byte in digest {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}
