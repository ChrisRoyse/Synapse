//! Atomic manifest and recovery ordering for Aster vaults.

mod error;
mod quarantine;

use crate::dedup::DedupPolicy;
use crate::sst::shared_reader;
use crate::timetravel::RetentionHorizon;
use crate::wal::{ReplayRecord, TornTail, replay_dir_after};
use calyx_core::{CalyxError, Result, TemporalPolicy};
use calyx_ledger::QuarantineSet;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

use error::{format_version_unsupported, storage_error};

const CURRENT_FILE: &str = "CURRENT";
const MANIFEST_FILE: &str = "MANIFEST";
const MANIFEST_PREFIX: &str = "manifest-";
const MANIFEST_SUFFIX: &str = ".json";
const MANIFEST_GENERATIONS_RETAINED: usize = 32;
const SUPPORTED_MANIFEST_MAJOR: u16 = 1;
const SUPPORTED_MANIFEST_MINOR: u16 = 0;
/// Watermark model 3 tracks the exact panel generation whose persistent-search
/// inputs changed. Model 2 tracked the right CFs but collapsed every panel into
/// one vault-global watermark; older models must be migrated conservatively
/// during vault recovery (issues #1808 and #1841).
pub(crate) const PERSISTENT_SEARCH_CONTENT_MODEL: u16 = 3;
const GLOBAL_PERSISTENT_SEARCH_CONTENT_MODEL: u16 = 2;

pub use quarantine::QuarantineRecord;

/// Version guard for MANIFEST bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestVersion {
    pub major: u16,
    pub minor: u16,
}

impl ManifestVersion {
    pub const fn current() -> Self {
        Self {
            major: SUPPORTED_MANIFEST_MAJOR,
            minor: SUPPORTED_MANIFEST_MINOR,
        }
    }

    fn validate(self) -> Result<()> {
        if self.major != SUPPORTED_MANIFEST_MAJOR {
            return Err(format_version_unsupported(format!(
                "unsupported MANIFEST major version {}; supported major is {}",
                self.major, SUPPORTED_MANIFEST_MAJOR
            )));
        }
        Ok(())
    }
}

/// Content-addressed immutable reference captured by a manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImmutableRef {
    pub logical_path: String,
    pub blake3_hex: String,
}

impl ImmutableRef {
    pub fn from_bytes(logical_path: impl Into<String>, bytes: &[u8]) -> Result<Self> {
        let hash = blake3::hash(bytes).to_hex().to_string();
        Self::new(logical_path, hash)
    }

    pub fn new(logical_path: impl Into<String>, blake3_hex: impl Into<String>) -> Result<Self> {
        let reference = Self {
            logical_path: logical_path.into(),
            blake3_hex: blake3_hex.into().to_ascii_lowercase(),
        };
        reference.validate()?;
        Ok(reference)
    }

    fn validate(&self) -> Result<()> {
        if self.logical_path.is_empty() || self.logical_path.starts_with('/') {
            return Err(CalyxError::aster_corrupt_shard(
                "manifest immutable ref path must be vault-relative",
            ));
        }
        if Path::new(&self.logical_path)
            .components()
            .any(invalid_component)
        {
            return Err(CalyxError::aster_corrupt_shard(
                "manifest immutable ref path escapes vault",
            ));
        }
        if self.logical_path == CURRENT_FILE
            || self.logical_path == MANIFEST_FILE
            || self.logical_path.ends_with(".tmp")
        {
            return Err(CalyxError::aster_corrupt_shard(
                "manifest immutable ref points at mutable control file",
            ));
        }
        if self.blake3_hex.len() != 64 || !self.blake3_hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(CalyxError::aster_corrupt_shard(
                "manifest immutable ref hash must be 32-byte hex blake3",
            ));
        }
        Ok(())
    }
}

/// Durable Aster vault manifest.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VaultManifest {
    pub version: ManifestVersion,
    pub manifest_seq: u64,
    pub durable_seq: u64,
    /// Max checkpointed seq (<= `durable_seq`) whose commit wrote at least one
    /// row in a CF that feeds derived search content (issue #1100). `None` on
    /// manifests written before this field existed; readers fail closed to
    /// `durable_seq` via [`Self::effective_derived_content_seq`].
    #[serde(default)]
    pub derived_content_seq: Option<u64>,
    /// Semantic model for `derived_content_seq`. `None` identifies manifests
    /// written by the original broad-CF classifier and requires an exact
    /// physical migration during recovery; unknown explicit models fail.
    #[serde(default)]
    pub derived_content_model: Option<u16>,
    /// Max checkpointed persistent-search-input seq for each exact panel
    /// generation. Entries are monotone and retained even after the panel's
    /// final Base row is deleted, so restart cannot forget a deletion that
    /// invalidated an older persisted index (issue #1841).
    #[serde(default)]
    pub panel_content_seqs: BTreeMap<u32, u64>,
    pub panel_ref: ImmutableRef,
    #[serde(default)]
    pub registry_ref: Option<ImmutableRef>,
    pub codebook_refs: Vec<ImmutableRef>,
    #[serde(default)]
    pub temporal_policy: Option<TemporalPolicy>,
    #[serde(default)]
    pub dedup_policy: Option<DedupPolicy>,
    #[serde(default)]
    pub retention_horizon: RetentionHorizon,
    pub degraded_rebuildable: bool,
    #[serde(default)]
    pub quarantines: Vec<QuarantineRecord>,
}

impl VaultManifest {
    pub fn new(
        manifest_seq: u64,
        durable_seq: u64,
        panel_ref: ImmutableRef,
        codebook_refs: Vec<ImmutableRef>,
    ) -> Result<Self> {
        Self::new_with_temporal_policy(
            manifest_seq,
            durable_seq,
            panel_ref,
            codebook_refs,
            Some(TemporalPolicy::default()),
        )
    }

    pub fn new_with_temporal_policy(
        manifest_seq: u64,
        durable_seq: u64,
        panel_ref: ImmutableRef,
        codebook_refs: Vec<ImmutableRef>,
        temporal_policy: Option<TemporalPolicy>,
    ) -> Result<Self> {
        Self::new_with_policies(
            manifest_seq,
            durable_seq,
            panel_ref,
            codebook_refs,
            temporal_policy,
            Some(DedupPolicy::default()),
        )
    }

    pub fn new_with_policies(
        manifest_seq: u64,
        durable_seq: u64,
        panel_ref: ImmutableRef,
        codebook_refs: Vec<ImmutableRef>,
        temporal_policy: Option<TemporalPolicy>,
        dedup_policy: Option<DedupPolicy>,
    ) -> Result<Self> {
        let manifest = Self {
            version: ManifestVersion::current(),
            manifest_seq,
            durable_seq,
            derived_content_seq: None,
            derived_content_model: Some(PERSISTENT_SEARCH_CONTENT_MODEL),
            panel_content_seqs: BTreeMap::new(),
            panel_ref,
            registry_ref: None,
            codebook_refs,
            temporal_policy,
            dedup_policy,
            retention_horizon: RetentionHorizon::default(),
            degraded_rebuildable: false,
            quarantines: Vec::new(),
        };
        manifest.validate()?;
        Ok(manifest)
    }

    /// Derived-content watermark this manifest vouches for. Legacy manifests
    /// (field absent) fail closed to `durable_seq`: every checkpointed seq is
    /// assumed to have changed derived-search inputs, which reproduces the
    /// pre-#1100 exact-equality freshness behavior — never laxer.
    pub fn effective_derived_content_seq(&self) -> u64 {
        self.derived_content_seq.unwrap_or(self.durable_seq)
    }

    pub(crate) fn uses_persistent_search_content_model(&self) -> bool {
        self.derived_content_model == Some(PERSISTENT_SEARCH_CONTENT_MODEL)
    }

    pub fn validate(&self) -> Result<()> {
        self.version.validate()?;
        if self.manifest_seq == 0 {
            return Err(CalyxError::aster_corrupt_shard(
                "manifest sequence must start at one",
            ));
        }
        if let Some(derived_content_seq) = self.derived_content_seq
            && derived_content_seq > self.durable_seq
        {
            return Err(CalyxError::aster_corrupt_shard(format!(
                "manifest derived_content_seq {derived_content_seq} exceeds durable_seq {}; the watermark can only vouch for checkpointed seqs",
                self.durable_seq
            )));
        }
        if let Some(model) = self.derived_content_model
            && !matches!(
                model,
                GLOBAL_PERSISTENT_SEARCH_CONTENT_MODEL | PERSISTENT_SEARCH_CONTENT_MODEL
            )
        {
            return Err(format_version_unsupported(format!(
                "unsupported derived-content watermark model {model}; supported models are {GLOBAL_PERSISTENT_SEARCH_CONTENT_MODEL} (migrated on open) and {PERSISTENT_SEARCH_CONTENT_MODEL}"
            )));
        }
        if let Some((panel_version, panel_content_seq)) = self
            .panel_content_seqs
            .iter()
            .find(|(_, panel_content_seq)| **panel_content_seq > self.durable_seq)
        {
            return Err(CalyxError::aster_corrupt_shard(format!(
                "manifest panel {panel_version} content seq {panel_content_seq} exceeds durable_seq {}; a panel watermark can only vouch for checkpointed seqs",
                self.durable_seq
            )));
        }
        self.panel_ref.validate()?;
        require_prefix(&self.panel_ref, "panel/")?;
        if let Some(reference) = &self.registry_ref {
            reference.validate()?;
            require_prefix(reference, "registry/")?;
        }
        let mut seen = BTreeSet::new();
        for reference in &self.codebook_refs {
            reference.validate()?;
            require_prefix(reference, "codebooks/")?;
            if !seen.insert(reference.logical_path.as_str()) {
                return Err(CalyxError::aster_corrupt_shard(
                    "manifest contains duplicate codebook ref",
                ));
            }
        }
        for quarantine in &self.quarantines {
            quarantine.validate()?;
        }
        if let Some(policy) = &self.temporal_policy {
            policy.validate()?;
        }
        if let Some(policy) = &self.dedup_policy {
            policy.validate_manifest()?;
        }
        self.retention_horizon.validate()?;
        Ok(())
    }
}

/// Atomic manifest writer/reader rooted at one vault directory.
#[derive(Debug, Clone)]
pub struct ManifestStore {
    vault_dir: PathBuf,
}

impl ManifestStore {
    pub fn open(vault_dir: impl AsRef<Path>) -> Self {
        Self {
            vault_dir: vault_dir.as_ref().to_path_buf(),
        }
    }

    pub fn write_current(&self, manifest: &VaultManifest) -> Result<ManifestWrite> {
        manifest.validate()?;
        fs::create_dir_all(&self.vault_dir).map_err(|error| {
            storage_error("create vault manifest directory", &self.vault_dir, error)
        })?;
        let lock_dir = self.vault_dir.join("locks");
        fs::create_dir_all(&lock_dir).map_err(|error| {
            storage_error("create vault manifest lock directory", &lock_dir, error)
        })?;
        // Every manifest writer, including callers that use ManifestStore
        // directly, shares this publication boundary. Vault checkpoint and
        // retention code deliberately take their higher-level checkpoint lock
        // first, so the order is always checkpoint -> manifest and manifest
        // fsync never requires the unrelated global durable commit lock.
        let _publish_guard =
            crate::file_lock::FileLockGuard::acquire(&lock_dir.join("manifest.publish.lock"))?;
        let pointer = manifest_filename(manifest.manifest_seq);
        let manifest_path = self.vault_dir.join(&pointer);
        let mirror_path = self.vault_dir.join(MANIFEST_FILE);
        let current_path = self.vault_dir.join(CURRENT_FILE);
        let bytes = encode_manifest(manifest)?;

        if current_path.exists() {
            let current = self.load_current()?;
            let expected_seq = current.manifest_seq.checked_add(1).ok_or_else(|| {
                CalyxError::ledger_chain_broken(
                    "manifest publication sequence exhausted at u64::MAX",
                )
            })?;
            if manifest.manifest_seq != expected_seq {
                return Err(CalyxError::aster_corrupt_shard(format!(
                    "manifest publication is stale or non-contiguous: current generation {} requires next generation {expected_seq}, proposed generation {}; retry from the current manifest instead of overwriting immutable history",
                    current.manifest_seq, manifest.manifest_seq
                )));
            }
        } else if manifest.manifest_seq == 0 {
            return Err(CalyxError::aster_corrupt_shard(
                "initial manifest publication requires a non-zero generation",
            ));
        }

        write_atomic(&manifest_path, &bytes)?;
        write_atomic(&mirror_path, &bytes)?;
        write_atomic(&current_path, pointer.as_bytes())?;
        verify_manifest_publication(
            &manifest_path,
            &mirror_path,
            &current_path,
            &pointer,
            &bytes,
        )?;
        let retention = reclaim_old_manifest_generations(&self.vault_dir, &pointer)?;

        Ok(ManifestWrite {
            manifest_path,
            mirror_path,
            current_path,
            pointer,
            manifest_generations_before: retention.before,
            manifest_generations_after: retention.after,
            manifest_generations_reclaimed: retention.reclaimed,
        })
    }

    pub fn load_current(&self) -> Result<VaultManifest> {
        let current_path = self.vault_dir.join(CURRENT_FILE);
        let pointer_bytes = fs::read(&current_path)
            .map_err(|error| storage_error("read CURRENT", &current_path, error))?;
        let pointer = std::str::from_utf8(&pointer_bytes)
            .map_err(|error| CalyxError::aster_corrupt_shard(format!("CURRENT utf8: {error}")))?
            .trim();
        if !valid_manifest_filename(pointer) {
            return Err(CalyxError::aster_corrupt_shard(
                "CURRENT does not point at immutable manifest file",
            ));
        }
        let manifest_path = self.vault_dir.join(pointer);
        let bytes = fs::read(&manifest_path)
            .map_err(|error| storage_error("read pointed MANIFEST", &manifest_path, error))?;
        let manifest = decode_manifest(&bytes)?;
        verify_immutable_refs(&self.vault_dir, &manifest)?;
        Ok(manifest)
    }

    pub fn current_pointer(&self) -> Result<String> {
        let current_path = self.vault_dir.join(CURRENT_FILE);
        let pointer = fs::read_to_string(&current_path)
            .map_err(|error| storage_error("read CURRENT", &current_path, error))?;
        Ok(pointer.trim().to_string())
    }

    pub fn append_quarantine(&self, record: QuarantineRecord) -> Result<VaultManifest> {
        let mut manifest = self.load_current()?;
        if !manifest.quarantines.contains(&record) {
            manifest.quarantines.push(record);
        }
        manifest.manifest_seq = manifest
            .manifest_seq
            .checked_add(1)
            .ok_or_else(|| CalyxError::ledger_chain_broken("manifest sequence exhausted"))?;
        self.write_current(&manifest)?;
        Ok(manifest)
    }
}

/// Files produced by an atomic manifest swap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestWrite {
    pub manifest_path: PathBuf,
    pub mirror_path: PathBuf,
    pub current_path: PathBuf,
    pub pointer: String,
    pub manifest_generations_before: usize,
    pub manifest_generations_after: usize,
    pub manifest_generations_reclaimed: usize,
}

/// Recovery result after loading MANIFEST first, then replaying WAL past it.
#[derive(Debug, Clone, PartialEq)]
pub struct RecoveryOutcome {
    pub manifest: VaultManifest,
    pub wal_records: Vec<ReplayRecord>,
    pub torn_tail: Option<TornTail>,
    pub last_recovered_seq: u64,
    pub degraded_rebuildable: bool,
}

/// Manifest plus header-only WAL-tip recovery used before bounded streaming
/// replay. Payload checks remain mandatory in the subsequent stream.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RecoveryMetadataOutcome {
    pub manifest: VaultManifest,
    pub torn_tail: Option<TornTail>,
    pub last_recovered_seq: u64,
    pub degraded_rebuildable: bool,
}

pub(crate) fn recover_vault_metadata(
    vault_dir: impl AsRef<Path>,
) -> Result<RecoveryMetadataOutcome> {
    let vault_dir = vault_dir.as_ref();
    let manifest = ManifestStore::open(vault_dir).load_current()?;
    let (wal_tip, torn_tail) = crate::wal::recover_tip_and_torn(vault_dir.join("wal"))?;
    Ok(RecoveryMetadataOutcome {
        last_recovered_seq: wal_tip.max(manifest.durable_seq),
        degraded_rebuildable: manifest.degraded_rebuildable,
        manifest,
        torn_tail,
    })
}

pub fn recover_vault(vault_dir: impl AsRef<Path>) -> Result<RecoveryOutcome> {
    let vault_dir = vault_dir.as_ref();
    let manifest = ManifestStore::open(vault_dir).load_current()?;
    let replay = replay_dir_after(vault_dir.join("wal"), manifest.durable_seq)?;
    let wal_records: Vec<_> = replay
        .records
        .into_iter()
        .filter(|record| record.seq > manifest.durable_seq)
        .collect();
    let last_recovered_seq = wal_records
        .last()
        .map_or(manifest.durable_seq, |record| record.seq);
    let degraded_rebuildable = manifest.degraded_rebuildable;

    Ok(RecoveryOutcome {
        manifest,
        wal_records,
        torn_tail: replay.torn_tail,
        last_recovered_seq,
        degraded_rebuildable,
    })
}

/// Reads a base CF shard through the fail-closed SST path.
pub fn read_base_shard(path: impl AsRef<Path>, key: &[u8]) -> Result<Option<Vec<u8>>> {
    shared_reader(path.as_ref())?.get(key)
}

pub fn is_quarantined(manifest: &VaultManifest, seq: u64) -> bool {
    manifest
        .quarantines
        .iter()
        .any(|record| record.contains(seq))
}

pub fn is_vault_seq_quarantined(vault_dir: impl AsRef<Path>, seq: u64) -> Result<bool> {
    let vault_dir = vault_dir.as_ref();
    if !vault_dir.join(CURRENT_FILE).exists() {
        return Ok(false);
    }
    let manifest = ManifestStore::open(vault_dir).load_current()?;
    Ok(is_quarantined(&manifest, seq))
}

/// Loads and cryptographically validates one current manifest generation,
/// then materializes its quarantine ranges for request-local lookups.
///
/// A `CURRENT` change after this function returns does not mutate the returned
/// set; the next call observes and validates the new pointer. A vault without
/// `CURRENT` preserves the legacy empty-quarantine behavior.
pub fn load_vault_quarantine_snapshot(vault_dir: impl AsRef<Path>) -> Result<QuarantineSet> {
    let vault_dir = vault_dir.as_ref();
    if !vault_dir.join(CURRENT_FILE).exists() {
        return Ok(QuarantineSet::default());
    }
    let manifest = ManifestStore::open(vault_dir).load_current()?;
    QuarantineSet::from_ranges(
        manifest
            .quarantines
            .iter()
            .map(|record| record.range_start..record.range_end),
    )
}

fn manifest_filename(seq: u64) -> String {
    format!("{MANIFEST_PREFIX}{seq:020}{MANIFEST_SUFFIX}")
}

fn manifest_sequence_from_filename(name: &str) -> Option<u64> {
    let digits = name
        .strip_prefix(MANIFEST_PREFIX)?
        .strip_suffix(MANIFEST_SUFFIX)?;
    if digits.len() != 20 || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

fn valid_manifest_filename(name: &str) -> bool {
    if !name.starts_with(MANIFEST_PREFIX) || !name.ends_with(MANIFEST_SUFFIX) {
        return false;
    }
    let digits = &name[MANIFEST_PREFIX.len()..name.len() - MANIFEST_SUFFIX.len()];
    !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ManifestRetention {
    before: usize,
    after: usize,
    reclaimed: usize,
}

fn verify_manifest_publication(
    manifest_path: &Path,
    mirror_path: &Path,
    current_path: &Path,
    pointer: &str,
    expected_manifest: &[u8],
) -> Result<()> {
    let immutable = fs::read(manifest_path).map_err(|error| {
        storage_error(
            "read immutable manifest after publish",
            manifest_path,
            error,
        )
    })?;
    if immutable != expected_manifest {
        return Err(CalyxError::aster_corrupt_shard(format!(
            "immutable manifest readback differs after publish: {}",
            manifest_path.display()
        )));
    }
    let mirror = fs::read(mirror_path)
        .map_err(|error| storage_error("read MANIFEST mirror after publish", mirror_path, error))?;
    if mirror != expected_manifest {
        return Err(CalyxError::aster_corrupt_shard(format!(
            "MANIFEST mirror readback differs after publish: {}",
            mirror_path.display()
        )));
    }
    let current = fs::read(current_path)
        .map_err(|error| storage_error("read CURRENT after publish", current_path, error))?;
    if current != pointer.as_bytes() {
        return Err(CalyxError::aster_corrupt_shard(format!(
            "CURRENT readback differs after publish: expected {pointer:?}, got {:?}",
            String::from_utf8_lossy(&current)
        )));
    }
    Ok(())
}

fn reclaim_old_manifest_generations(
    vault_dir: &Path,
    current_pointer: &str,
) -> Result<ManifestRetention> {
    let mut generations = Vec::new();
    let entries = fs::read_dir(vault_dir)
        .map_err(|error| storage_error("list manifest generations", vault_dir, error))?;
    for entry in entries {
        let entry = entry
            .map_err(|error| storage_error("read manifest generation entry", vault_dir, error))?;
        let file_type = entry.file_type().map_err(|error| {
            storage_error("read manifest generation file type", &entry.path(), error)
        })?;
        if !file_type.is_file() {
            continue;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(sequence) = manifest_sequence_from_filename(name) else {
            continue;
        };
        generations.push((sequence, name.to_owned(), entry.path()));
    }
    generations.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| right.1.cmp(&left.1)));
    let before = generations.len();
    if before <= MANIFEST_GENERATIONS_RETAINED {
        return Ok(ManifestRetention {
            before,
            after: before,
            reclaimed: 0,
        });
    }

    let mut retained = BTreeSet::from([current_pointer.to_owned()]);
    for (_, name, _) in &generations {
        if retained.len() >= MANIFEST_GENERATIONS_RETAINED {
            break;
        }
        retained.insert(name.clone());
    }
    if !retained.contains(current_pointer) {
        return Err(CalyxError::aster_corrupt_shard(format!(
            "manifest retention cannot prove CURRENT target {current_pointer:?} is retained"
        )));
    }

    let mut reclaimed = 0usize;
    for (_, name, path) in generations {
        if retained.contains(&name) {
            continue;
        }
        fs::remove_file(&path)
            .map_err(|error| storage_error("reclaim old manifest generation", &path, error))?;
        reclaimed = reclaimed.saturating_add(1);
    }
    crate::fsync::sync_dir(vault_dir, "manifest generation retention")?;
    let after = before.saturating_sub(reclaimed);
    tracing::info!(
        code = "CALYX_ASTER_MANIFEST_GENERATIONS_RECLAIMED",
        current_pointer,
        generations_before = before,
        generations_after = after,
        generations_reclaimed = reclaimed,
        retained_limit = MANIFEST_GENERATIONS_RETAINED,
        source_of_truth =
            "CURRENT + exact manifest-<20 digit seq>.json files + synced vault directory",
        "reclaimed old immutable manifest generations after durable CURRENT readback"
    );
    Ok(ManifestRetention {
        before,
        after,
        reclaimed,
    })
}

fn encode_manifest(manifest: &VaultManifest) -> Result<Vec<u8>> {
    serde_json::to_vec_pretty(manifest)
        .map_err(|error| CalyxError::aster_corrupt_shard(format!("encode MANIFEST: {error}")))
}

fn decode_manifest(bytes: &[u8]) -> Result<VaultManifest> {
    let manifest: VaultManifest = serde_json::from_slice(bytes)
        .map_err(|error| CalyxError::aster_corrupt_shard(format!("decode MANIFEST: {error}")))?;
    manifest.validate()?;
    Ok(manifest)
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    crate::fsync::write_atomic_replace(path, bytes, "manifest")
}

fn invalid_component(component: Component<'_>) -> bool {
    matches!(
        component,
        Component::ParentDir | Component::RootDir | Component::Prefix(_)
    )
}

fn require_prefix(reference: &ImmutableRef, prefix: &str) -> Result<()> {
    if !reference.logical_path.starts_with(prefix) {
        return Err(CalyxError::aster_corrupt_shard(format!(
            "manifest ref {} must be under {prefix}",
            reference.logical_path
        )));
    }
    Ok(())
}

fn verify_immutable_refs(vault_dir: &Path, manifest: &VaultManifest) -> Result<()> {
    verify_immutable_ref(vault_dir, &manifest.panel_ref)?;
    if let Some(reference) = &manifest.registry_ref {
        verify_immutable_ref(vault_dir, reference)?;
    }
    for reference in &manifest.codebook_refs {
        verify_immutable_ref(vault_dir, reference)?;
    }
    Ok(())
}

fn verify_immutable_ref(vault_dir: &Path, reference: &ImmutableRef) -> Result<()> {
    reference.validate()?;
    let path = vault_dir.join(&reference.logical_path);
    let bytes = fs::read(&path).map_err(|error| {
        CalyxError::aster_corrupt_shard(format!(
            "manifest immutable ref {} unreadable: {error}",
            reference.logical_path
        ))
    })?;
    let actual = blake3::hash(&bytes).to_hex().to_string();
    if actual != reference.blake3_hex {
        return Err(CalyxError::aster_corrupt_shard(format!(
            "manifest immutable ref {} hash mismatch: expected {}, got {}",
            reference.logical_path, reference.blake3_hex, actual
        )));
    }
    Ok(())
}
