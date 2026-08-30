#[path = "persisted/dense.rs"]
mod dense;
#[path = "persisted/filter.rs"]
mod filter;
#[path = "persisted/freshness.rs"]
mod freshness;
#[path = "persisted/generation.rs"]
mod generation;
#[path = "persisted/marker.rs"]
pub mod marker;
#[path = "persisted/multi.rs"]
mod multi;
#[path = "persisted/pinned.rs"]
mod pinned;
#[path = "persisted/rebuild.rs"]
mod rebuild;
#[path = "persisted/rebuild_plan.rs"]
mod rebuild_plan;
#[path = "persisted/rebuild_stream.rs"]
mod rebuild_stream;
#[path = "persisted/sparse.rs"]
mod sparse;

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use calyx_aster::vault::AsterVault;
use calyx_core::{CalyxError, Constellation, CxId, SlotId, SlotVector};
use calyx_sextant::QueryFilters;
use calyx_sextant::index::IndexSearchHit;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::{CliError, CliResult};
pub use generation::{
    PersistedDenseQuantization, PersistedSearchGeneration, PersistedSearchSlot, slot_scoring_law,
};
pub use marker::{
    MarkerClearOutcome, REBUILD_REQUIRED_REMEDIATION, REBUILD_REQUIRED_SCHEMA,
    RebuildRequiredMarker, clear_rebuild_required_marker, clear_rebuild_required_marker_if_owned,
    read_rebuild_required_marker, rebuild_required_marker_path, write_rebuild_required_marker,
};
pub use multi::score_persisted_maxsim_pair;
pub(crate) use pinned::canonical_vault_dir as canonical_pin_vault_dir;
pub use rebuild::{
    CandidateSearchGeneration, RebuildProgress, load_docs,
    rebuild_candidate_for_vault_with_panel_state_and_dense_config,
    rebuild_candidate_for_vault_with_panel_state_and_dense_config_at_snapshot, rebuild_for_vault,
    rebuild_for_vault_with_fallible_progress, rebuild_for_vault_with_panel_state,
    rebuild_for_vault_with_panel_state_and_dense_config,
    rebuild_for_vault_with_panel_state_and_dense_config_at_snapshot,
    rebuild_for_vault_with_panel_state_dense_config_progress,
    rebuild_for_vault_with_panel_state_fallible_progress,
    rebuild_for_vault_with_panel_state_progress, rebuild_for_vault_with_progress,
};
pub use rebuild_stream::rebuild_panel_membership_for_vault;

const MANIFEST_FORMAT: &str = "calyx-search-index-manifest-v2";
const IDMAP_FORMAT: &str = "calyx-search-index-idmap-v2";
const INDEX_ROOT: &str = "idx/search";
const MANIFEST_NAME: &str = "manifest.json";
// Keep the currently queried generation plus one immediately prior runtime.
// Larger defaults pin mmap-backed artifacts long after publication and delay
// physical reclamation. An audited deployment may raise this, but the process
// still refuses an unbounded generation cache.
const DEFAULT_OPEN_GENERATION_CACHE_ENTRIES: usize = 2;
// Raised 4 -> 8 (operator-interference / OOM incident, 2026-08-30). A host with
// five declared-queryable panels could not fit its hot set in a 4-entry cache,
// so every maintenance walk evicted and re-materialized a generation. Eviction
// keeps the retired runtime alive until its last strong reference drops, so a
// fast evict/reopen cycle piles several multi-hundred-MB generations up at once:
// the observed daemon went from ~460 MB to 5.7 GB in ten seconds and was killed
// by its Job commit cap. Residency is bounded by the generations themselves
// (~2.5 GB for this host's whole set), which is far cheaper than the churn.
const MAX_OPEN_GENERATION_CACHE_ENTRIES: usize = 8;
const OPEN_GENERATION_CACHE_ENTRIES_ENV: &str = "CALYX_SEARCH_OPEN_GENERATION_CACHE_ENTRIES";
static OPEN_GENERATION_CACHE: OnceLock<Mutex<OpenGenerationCache>> = OnceLock::new();
pub const CALYX_SEARCH_PANEL_SCOPE_REQUIRED: &str = "CALYX_SEARCH_PANEL_SCOPE_REQUIRED";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistedSearchManifestArtifact {
    pub panel_version: u32,
    pub base_seq: u64,
    pub manifest_sha256: String,
    pub manifest_bytes: Vec<u8>,
}

/// Hash-verified membership of one panel-scoped search generation.
///
/// The filter sidecar is the compact secondary access path from
/// `(panel_version, CxId)` to Base. It contains one row for every Base row in
/// the panel, including records with no searchable vector, and is sealed by
/// the live manifest. Callers needing a current snapshot must pass it through
/// `reconcile_panel_membership`; the immutable ids alone describe `base_seq`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PersistedPanelMembership {
    pub panel_version: u32,
    pub base_seq: u64,
    pub manifest_sha256: String,
    pub sidecar_sha256: String,
    pub ids: Vec<CxId>,
}

/// Load-bearing parameters for persisted dense DiskANN generations.
///
/// Values are sealed into the immutable generation manifest and read back by
/// query-time open, so tuning cannot silently change an existing artifact.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PersistedDenseIndexConfig {
    pub m_max: usize,
    pub ef_construction: usize,
    pub beamwidth: usize,
    pub ef_search: usize,
    pub alpha: f32,
    /// Product-quantizer bits per subvector for explicitly selected slots.
    /// Unlisted slots remain exact; `4` and `8` require exact raw-sidecar
    /// reranking. Quantization is slot-scoped because some panel lanes are
    /// intentionally constant and cannot train a meaningful codebook.
    #[serde(default)]
    pub quant_bits_by_slot: BTreeMap<u16, u8>,
}

impl Default for PersistedDenseIndexConfig {
    fn default() -> Self {
        Self {
            m_max: 32,
            ef_construction: 64,
            beamwidth: 32,
            ef_search: 64,
            alpha: 1.2,
            quant_bits_by_slot: BTreeMap::new(),
        }
    }
}

impl Eq for PersistedDenseIndexConfig {}

impl PersistedDenseIndexConfig {
    pub fn validate(&self) -> CliResult<Self> {
        if self.m_max == 0
            || self.ef_construction == 0
            || self.beamwidth == 0
            || self.ef_search == 0
            || !self.alpha.is_finite()
            || self.alpha < 1.0
        {
            return Err(stale(format!(
                "invalid persisted dense index config m_max={} ef_construction={} beamwidth={} ef_search={} alpha={}; counts must be positive and alpha must be finite and >= 1.0",
                self.m_max, self.ef_construction, self.beamwidth, self.ef_search, self.alpha
            )));
        }
        if let Some((slot, bits)) = self
            .quant_bits_by_slot
            .iter()
            .find(|(_, bits)| !matches!(bits, 4 | 8))
        {
            return Err(stale(format!(
                "invalid persisted dense index quantization for slot {slot}: {bits} bits; explicitly selected slots must use 4 or 8 bits, and exact slots must be omitted"
            )));
        }
        Ok(self.clone())
    }

    pub fn quant_bits_for(&self, slot: SlotId) -> u8 {
        self.quant_bits_by_slot
            .get(&slot.get())
            .copied()
            .unwrap_or(32)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct SearchIndexManifest {
    format: String,
    panel_version: u32,
    base_seq: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    diskann_build_backend: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    diskann_build_backend_source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sextant_cuvs_compiled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sextant_cuda_pq_compiled: Option<bool>,
    #[serde(default)]
    dense_index_config: PersistedDenseIndexConfig,
    #[serde(default)]
    filter: Option<FilterIndexEntry>,
    slots: Vec<SearchIndexEntry>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct SearchIndexEntry {
    slot: u16,
    kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    dim: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    token_dim: Option<u32>,
    len: usize,
    built_at_seq: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    graph_rel: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    id_map_rel: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    index_rel: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    token_count: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    dense_quantization: Option<Box<DenseQuantizationEntry>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct DenseQuantizationEntry {
    bits: u8,
    subvectors: usize,
    centroids: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    build_diagnostics: Option<calyx_sextant::index::DiskAnnPqBuildDiagnostics>,
    pq_rel: String,
    pq_sha256: String,
    raw_rel: String,
    raw_sha256: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct SlotIdMap {
    format: String,
    panel_version: u32,
    slot: u16,
    ids: Vec<CxId>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct FilterIndexEntry {
    built_at_seq: u64,
    len: usize,
    index_rel: String,
    sha256: String,
}

#[derive(Clone, Debug)]
struct RebuildSummary {
    total_rows: usize,
    manifest_path: PathBuf,
    base_seq: u64,
}

#[derive(Debug)]
pub struct PersistedSearchIndexes {
    vault_dir: PathBuf,
    manifest: SearchIndexManifest,
    manifest_sha256: String,
    runtime: Arc<PersistedSearchRuntime>,
    runtime_cache_hit: bool,
}

#[derive(Debug)]
struct PersistedSearchRuntime {
    dense: dense::DenseIndexCache,
    artifact_roots: BTreeSet<PathBuf>,
}

impl PersistedSearchRuntime {
    fn new(artifact_roots: BTreeSet<PathBuf>) -> Self {
        Self {
            dense: dense::DenseIndexCache::default(),
            artifact_roots,
        }
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct OpenGenerationKey {
    vault_dir: String,
    panel_version: u32,
    manifest_sha256: String,
}

#[derive(Debug)]
struct OpenGenerationCache {
    entries: BTreeMap<OpenGenerationKey, Arc<PersistedSearchRuntime>>,
    order: VecDeque<OpenGenerationKey>,
    retired: Vec<Weak<PersistedSearchRuntime>>,
    max_entries: usize,
}

impl OpenGenerationCache {
    fn new(max_entries: usize) -> Self {
        Self {
            entries: BTreeMap::new(),
            order: VecDeque::new(),
            retired: Vec::new(),
            max_entries,
        }
    }

    fn touch(&mut self, key: &OpenGenerationKey) {
        self.order.retain(|candidate| candidate != key);
        self.order.push_back(key.clone());
    }

    fn get(&mut self, key: &OpenGenerationKey) -> Option<Arc<PersistedSearchRuntime>> {
        let runtime = self.entries.get(key).cloned()?;
        self.touch(key);
        Some(runtime)
    }

    fn get_or_insert(
        &mut self,
        key: OpenGenerationKey,
        artifact_roots: BTreeSet<PathBuf>,
    ) -> (Arc<PersistedSearchRuntime>, bool, Option<OpenGenerationKey>) {
        if let Some(runtime) = self.entries.get(&key).cloned() {
            self.touch(&key);
            return (runtime, true, None);
        }
        let runtime = Arc::new(PersistedSearchRuntime::new(artifact_roots));
        self.entries.insert(key.clone(), Arc::clone(&runtime));
        self.touch(&key);
        let mut evicted = None;
        while self.entries.len() > self.max_entries {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            if oldest == key {
                self.order.push_back(oldest);
                continue;
            }
            if let Some(old_runtime) = self.entries.remove(&oldest) {
                self.retired.push(Arc::downgrade(&old_runtime));
                evicted = Some(oldest);
            }
        }
        self.retired.retain(|runtime| runtime.strong_count() > 0);
        (runtime, false, evicted)
    }
}

fn read_open_generation_cache_capacity() -> Result<(usize, Option<String>), String> {
    match std::env::var(OPEN_GENERATION_CACHE_ENTRIES_ENV) {
        Ok(raw) => {
            let parsed = raw.parse::<usize>().map_err(|error| {
                format!(
                    "{OPEN_GENERATION_CACHE_ENTRIES_ENV} must be an integer in 1..={MAX_OPEN_GENERATION_CACHE_ENTRIES}: value={raw:?} error={error}"
                )
            })?;
            if !(1..=MAX_OPEN_GENERATION_CACHE_ENTRIES).contains(&parsed) {
                return Err(format!(
                    "{OPEN_GENERATION_CACHE_ENTRIES_ENV} must be in 1..={MAX_OPEN_GENERATION_CACHE_ENTRIES}: value={parsed}"
                ));
            }
            Ok((parsed, Some(raw)))
        }
        Err(std::env::VarError::NotPresent) => Ok((DEFAULT_OPEN_GENERATION_CACHE_ENTRIES, None)),
        Err(std::env::VarError::NotUnicode(_)) => Err(format!(
            "{OPEN_GENERATION_CACHE_ENTRIES_ENV} is not valid UTF-8"
        )),
    }
}

fn open_generation_cache_capacity() -> CliResult<usize> {
    static CAPACITY: OnceLock<(usize, Option<String>)> = OnceLock::new();
    let observed = read_open_generation_cache_capacity().map_err(|detail| CliError::usage(format!(
            "CALYX_SEARCH_OPEN_GENERATION_CACHE_CONFIG_INVALID: {detail}; remediation=set {OPEN_GENERATION_CACHE_ENTRIES_ENV} to an integer in 1..={MAX_OPEN_GENERATION_CACHE_ENTRIES} or unset it for the bounded default {DEFAULT_OPEN_GENERATION_CACHE_ENTRIES}"
        )))?;
    let frozen = CAPACITY.get_or_init(|| observed.clone());
    if frozen != &observed {
        return Err(CliError::usage(format!(
            "CALYX_SEARCH_OPEN_GENERATION_CACHE_CONFIG_DRIFT: frozen value {:?} (capacity {}) differs from current value {:?} (capacity {}); remediation=restart the process after changing {OPEN_GENERATION_CACHE_ENTRIES_ENV}",
            frozen.1, frozen.0, observed.1, observed.0
        )));
    }
    Ok(frozen.0)
}

fn open_generation_artifact_roots(
    vault_dir: &Path,
    manifest: &SearchIndexManifest,
) -> CliResult<BTreeSet<PathBuf>> {
    let canonical_vault = PathBuf::from(canonical_pin_vault_dir(vault_dir)?);
    let mut roots = BTreeSet::new();
    for entry in manifest
        .slots
        .iter()
        .filter(|entry| entry.kind == "diskann")
    {
        let slot = SlotId::new(entry.slot);
        let graph = fs::canonicalize(canonical_vault.join(entry.require_graph_rel(slot)?))?;
        if !graph.starts_with(&canonical_vault) {
            return Err(CliError::io(format!(
                "CALYX_SEARCH_GENERATION_ARTIFACT_OUTSIDE_VAULT: panel={} slot={} artifact={}; remediation=rebuild the manifest with vault-relative immutable artifacts",
                manifest.panel_version,
                entry.slot,
                graph.display()
            )));
        }
        let root = graph.parent().ok_or_else(|| {
            CliError::io(format!(
                "CALYX_SEARCH_GENERATION_ARTIFACT_ROOT_MISSING: panel={} slot={} artifact={}; remediation=rebuild the malformed generation",
                manifest.panel_version,
                entry.slot,
                graph.display()
            ))
        })?;
        roots.insert(root.to_path_buf());
    }
    Ok(roots)
}

/// Canonical artifact directories that remain reachable through an open mmap
/// runtime. Rebuild pruning must retain these until the final query/reference
/// drops; otherwise Windows refuses deletion and platforms that permit unlink
/// can violate the mmap immutability contract if a path is rebuilt in place.
pub(super) fn retained_open_generation_artifact_roots_for_prune(
    root: &Path,
    keep: &[PathBuf],
) -> CliResult<BTreeSet<PathBuf>> {
    let Some(cache) = OPEN_GENERATION_CACHE.get() else {
        return Ok(BTreeSet::new());
    };
    let canonical_root = fs::canonicalize(root)?;
    let canonical_keep = keep
        .iter()
        .filter(|path| path.exists())
        .map(fs::canonicalize)
        .collect::<std::io::Result<BTreeSet<_>>>()?;
    let mut cache = cache.lock().map_err(|_| {
        CliError::io(
            "CALYX_SEARCH_OPEN_GENERATION_CACHE_POISONED: generation runtime cache lock was poisoned during pruning; remediation=restart the process and inspect the first panic",
        )
    })?;
    let superseded = cache
        .entries
        .iter()
        .filter_map(|(key, runtime)| {
            runtime
                .artifact_roots
                .iter()
                .any(|artifact| {
                    artifact.parent() == Some(canonical_root.as_path())
                        && !canonical_keep.contains(artifact)
                })
                .then_some(key.clone())
        })
        .collect::<Vec<_>>();
    for key in superseded {
        if let Some(runtime) = cache.entries.remove(&key) {
            cache.retired.push(Arc::downgrade(&runtime));
        }
        cache.order.retain(|candidate| candidate != &key);
    }
    let mut retained = BTreeSet::new();
    for runtime in cache.entries.values() {
        retained.extend(runtime.artifact_roots.iter().cloned());
    }
    cache.retired.retain(|runtime| {
        let Some(runtime) = runtime.upgrade() else {
            return false;
        };
        retained.extend(runtime.artifact_roots.iter().cloned());
        true
    });
    Ok(retained)
}

fn open_generation_runtime(
    vault_dir: &Path,
    panel_version: u32,
    manifest_sha256: &str,
    manifest: &SearchIndexManifest,
) -> CliResult<(Arc<PersistedSearchRuntime>, bool)> {
    let max_entries = open_generation_cache_capacity()?;
    let key = OpenGenerationKey {
        vault_dir: canonical_pin_vault_dir(vault_dir)?,
        panel_version,
        manifest_sha256: manifest_sha256.to_owned(),
    };
    let cache =
        OPEN_GENERATION_CACHE.get_or_init(|| Mutex::new(OpenGenerationCache::new(max_entries)));
    {
        let mut cache = cache.lock().map_err(|_| {
            CliError::io(
                "CALYX_SEARCH_OPEN_GENERATION_CACHE_POISONED: generation runtime cache lock was poisoned; remediation=restart the process and inspect the first panic",
            )
        })?;
        if cache.max_entries != max_entries {
            return Err(CliError::usage(format!(
                "CALYX_SEARCH_OPEN_GENERATION_CACHE_CONFIG_DRIFT: frozen capacity {} differs from requested {max_entries}; remediation=restart the process after changing {OPEN_GENERATION_CACHE_ENTRIES_ENV}",
                cache.max_entries
            )));
        }
        if let Some(runtime) = cache.get(&key) {
            tracing::info!(
                code = "CALYX_SEARCH_OPEN_GENERATION_CACHE_HIT",
                vault_dir = %key.vault_dir,
                panel_version,
                manifest_sha256,
                cache_entries = cache.entries.len(),
                cache_capacity = cache.max_entries,
                "resolved the immutable persisted-search generation runtime"
            );
            return Ok((runtime, true));
        }
    }
    let artifact_roots = open_generation_artifact_roots(vault_dir, manifest)?;
    let mut cache = cache.lock().map_err(|_| {
        CliError::io(
            "CALYX_SEARCH_OPEN_GENERATION_CACHE_POISONED: generation runtime cache lock was poisoned; remediation=restart the process and inspect the first panic",
        )
    })?;
    let (runtime, hit, evicted) = cache.get_or_insert(key.clone(), artifact_roots);
    tracing::info!(
        code = if hit {
            "CALYX_SEARCH_OPEN_GENERATION_CACHE_HIT"
        } else {
            "CALYX_SEARCH_OPEN_GENERATION_CACHE_MISS"
        },
        vault_dir = %key.vault_dir,
        panel_version,
        manifest_sha256,
        cache_entries = cache.entries.len(),
        cache_capacity = cache.max_entries,
        evicted_panel_version = evicted.as_ref().map(|entry| entry.panel_version),
        evicted_manifest_sha256 = evicted.as_ref().map(|entry| entry.manifest_sha256.as_str()),
        "resolved the immutable persisted-search generation runtime"
    );
    Ok((runtime, hit))
}

impl PersistedSearchIndexes {
    pub fn open(vault_dir: &Path, panel_version: u32) -> CliResult<Self> {
        let manifest_path = manifest_path(vault_dir, panel_version);
        if !manifest_path.is_file() {
            let legacy = legacy_manifest_path(vault_dir);
            if legacy.is_file() {
                return Err(stale(format!(
                    "legacy bare-slot search manifest exists at {} but panel {panel_version} requires {}; rebuild from Base into a panel-scoped v2 generation",
                    legacy.display(),
                    manifest_path.display()
                )));
            }
            return Err(stale(format!(
                "persistent search index manifest missing at {}; ingest or rebuild the vault before search{}",
                manifest_path.display(),
                marker::marker_error_context(vault_dir, panel_version)
            )));
        }
        Self::open_manifest_path(vault_dir, panel_version, &manifest_path)
    }

    /// Opens one immutable candidate generation built for an Anneal shadow.
    ///
    /// Candidate paths are derived here rather than accepted from callers, so
    /// a persisted tuning artifact can identify a generation without turning
    /// an arbitrary filesystem path into trusted search input.
    pub fn open_candidate(
        vault_dir: &Path,
        panel_version: u32,
        candidate_key: [u8; 32],
        base_seq: u64,
    ) -> CliResult<Self> {
        let manifest_path =
            candidate_manifest_path(vault_dir, panel_version, candidate_key, base_seq);
        if !manifest_path.is_file() {
            return Err(stale(format!(
                "candidate search index manifest missing at {}; rebuild the exact content-addressed shadow generation before evaluation",
                manifest_path.display()
            )));
        }
        let opened = Self::open_manifest_path(vault_dir, panel_version, &manifest_path)?;
        if opened.manifest.base_seq != base_seq {
            return Err(stale(format!(
                "candidate search manifest {} declares Base seq {}, expected pinned seq {base_seq}",
                manifest_path.display(),
                opened.manifest.base_seq
            )));
        }
        Ok(opened)
    }

    fn open_manifest_path(
        vault_dir: &Path,
        panel_version: u32,
        manifest_path: &Path,
    ) -> CliResult<Self> {
        let manifest_bytes = fs::read(manifest_path)?;
        Self::from_manifest_bytes(vault_dir, panel_version, &manifest_bytes)
    }

    fn from_manifest_bytes(
        vault_dir: &Path,
        panel_version: u32,
        manifest_bytes: &[u8],
    ) -> CliResult<Self> {
        let manifest_sha256 = sha256_hex(manifest_bytes);
        let manifest: SearchIndexManifest = serde_json::from_slice(manifest_bytes)?;
        if manifest.format != MANIFEST_FORMAT {
            return Err(stale(format!(
                "persistent search index manifest bytes have format {}; expected {MANIFEST_FORMAT}",
                manifest.format,
            )));
        }
        if manifest.panel_version != panel_version {
            return Err(stale(format!(
                "persistent search index manifest bytes declare panel {} but panel {panel_version} was requested; rebuild the exact panel generation",
                manifest.panel_version
            )));
        }
        let (runtime, runtime_cache_hit) =
            open_generation_runtime(vault_dir, panel_version, &manifest_sha256, &manifest)?;
        Ok(Self {
            vault_dir: vault_dir.to_path_buf(),
            manifest,
            manifest_sha256,
            runtime,
            runtime_cache_hit,
        })
    }

    fn manifest_artifact(&self) -> CliResult<PersistedSearchManifestArtifact> {
        let manifest_bytes = serde_json::to_vec_pretty(&self.manifest)?;
        let manifest_sha256 = sha256_hex(&manifest_bytes);
        if manifest_sha256 != self.manifest_sha256 {
            return Err(stale(format!(
                "parsed search manifest canonical bytes hash {manifest_sha256}, but the opened physical bytes hash {}; manifest serialization is not byte-stable",
                self.manifest_sha256
            )));
        }
        Ok(PersistedSearchManifestArtifact {
            panel_version: self.manifest.panel_version,
            base_seq: self.manifest.base_seq,
            manifest_sha256,
            manifest_bytes,
        })
    }

    pub(crate) fn panel_version(&self) -> u32 {
        self.manifest.panel_version
    }

    pub fn search(
        &self,
        slot: SlotId,
        query: &SlotVector,
        k: usize,
    ) -> CliResult<Vec<IndexSearchHit>> {
        let entry = self.require_entry(slot)?;
        match query {
            SlotVector::Dense { .. } => dense::search(
                &self.vault_dir,
                entry,
                dense::DenseSearchContext {
                    panel_version: self.manifest.panel_version,
                    slot,
                    config: self.manifest.dense_index_config.validate()?,
                },
                query,
                k,
                &self.runtime.dense,
            ),
            SlotVector::Sparse { .. } => sparse::search(
                &self.vault_dir,
                entry,
                self.manifest.base_seq,
                slot,
                query,
                k,
                None,
            ),
            SlotVector::Multi { .. } => multi::search(
                &self.vault_dir,
                entry,
                self.manifest.base_seq,
                slot,
                query,
                k,
                None,
            ),
            SlotVector::Absent { .. } => Err(stale(format!(
                "persistent search slot {slot} received an absent query vector; remeasure the active panel"
            ))),
        }
    }

    /// Exhaustive exact-cosine reference search over one dense lane.
    ///
    /// This is intentionally separate from production ANN recall. Anneal uses
    /// it to derive held-out expected ranks from the immutable raw sidecar
    /// instead of accepting caller-asserted relevance labels.
    pub fn exact_dense_search(
        &self,
        slot: SlotId,
        query: &SlotVector,
        k: usize,
    ) -> CliResult<Vec<IndexSearchHit>> {
        let entry = self.require_entry(slot)?;
        dense::exact_search(
            &self.vault_dir,
            entry,
            dense::DenseSearchContext {
                panel_version: self.manifest.panel_version,
                slot,
                config: self.manifest.dense_index_config.validate()?,
            },
            query,
            k,
            &self.runtime.dense,
        )
    }

    /// Exact indexed identities for one dense lane, in durable id-map order.
    pub fn dense_ids(&self, slot: SlotId) -> CliResult<Vec<CxId>> {
        let entry = self.require_entry(slot)?;
        dense::ids(
            &self.vault_dir,
            entry,
            self.manifest.panel_version,
            slot,
            self.manifest.dense_index_config.validate()?,
            &self.runtime.dense,
        )
    }

    pub fn search_filtered(
        &self,
        slot: SlotId,
        query: &SlotVector,
        k: usize,
        candidates: &BTreeSet<CxId>,
    ) -> CliResult<Vec<IndexSearchHit>> {
        if candidates.is_empty() {
            return Ok(Vec::new());
        }
        let entry = self.require_entry(slot)?;
        match query {
            SlotVector::Dense { .. } => dense::search_filtered(
                &self.vault_dir,
                entry,
                dense::DenseSearchContext {
                    panel_version: self.manifest.panel_version,
                    slot,
                    config: self.manifest.dense_index_config.validate()?,
                },
                query,
                k,
                candidates,
                &self.runtime.dense,
            ),
            SlotVector::Sparse { .. } => sparse::search(
                &self.vault_dir,
                entry,
                self.manifest.base_seq,
                slot,
                query,
                k,
                Some(candidates),
            ),
            SlotVector::Multi { .. } => multi::search(
                &self.vault_dir,
                entry,
                self.manifest.base_seq,
                slot,
                query,
                k,
                Some(candidates),
            ),
            SlotVector::Absent { .. } => Err(stale(format!(
                "persistent filtered search slot {slot} received an absent query vector; remeasure the active panel"
            ))),
        }
    }

    /// Searches the immutable generation while replacing every key changed
    /// after its base sequence with that key's vector from one newer pinned
    /// MVCC snapshot. Changed tombstones/absent vectors are represented only
    /// in `changed` and therefore remove their stale indexed form (#1842).
    pub(crate) fn search_reconciled(
        &self,
        slot: SlotId,
        query: &SlotVector,
        k: usize,
        candidates: Option<&BTreeSet<CxId>>,
        changed: &BTreeSet<CxId>,
        replacements: &BTreeMap<CxId, SlotVector>,
    ) -> CliResult<Vec<IndexSearchHit>> {
        if changed.is_empty() {
            return match candidates {
                Some(candidates) => self.search_filtered(slot, query, k, candidates),
                None => self.search(slot, query, k),
            };
        }
        let entry = self.require_entry(slot)?;
        if matches!(query, SlotVector::Sparse { .. }) {
            return sparse::search_reconciled(
                &self.vault_dir,
                entry,
                self.manifest.base_seq,
                slot,
                query,
                k,
                candidates,
                changed,
                replacements,
            );
        }
        let fetch_k = k.saturating_add(changed.len()).min(entry.len);
        let indexed = match candidates {
            Some(candidates) => self.search_filtered(slot, query, fetch_k, candidates)?,
            None => self.search(slot, query, fetch_k)?,
        };
        let mut scored = indexed
            .into_iter()
            .filter(|hit| !changed.contains(&hit.cx_id))
            .map(|hit| (hit.cx_id, hit.score))
            .collect::<Vec<_>>();
        let replacement_scores = match query {
            SlotVector::Dense { .. } => {
                dense::score_replacements(slot, query, replacements, candidates)?
            }
            SlotVector::Multi { .. } => {
                multi::score_replacements(slot, query, replacements, candidates)?
            }
            SlotVector::Sparse { .. } => unreachable!("sparse returned above"),
            SlotVector::Absent { .. } => {
                return Err(stale(format!(
                    "delta-reconciled search slot {slot} received an absent query vector"
                )));
            }
        };
        scored.extend(replacement_scores);
        scored.sort_by(|left, right| {
            right
                .1
                .total_cmp(&left.1)
                .then_with(|| left.0.to_string().cmp(&right.0.to_string()))
        });
        scored.truncate(k);
        Ok(calyx_sextant::index::ranked(scored))
    }

    pub fn filter_candidates(&self, filters: &QueryFilters) -> CliResult<Option<BTreeSet<CxId>>> {
        filter::candidates(
            &self.vault_dir,
            self.manifest.filter.as_ref(),
            self.manifest.base_seq,
            filters,
        )
    }

    /// Reads and hash-verifies every Base identity in this exact panel.
    ///
    /// This is a secondary-index read, not a search query. The returned ids are
    /// strictly ordered by `CxId`, match the sidecar's declared row count, and
    /// have each been checked against the manifest panel. There is deliberately
    /// no global-Base fallback: an absent or corrupt sidecar is an unavailable
    /// panel access path and fails closed.
    pub fn panel_membership(&self) -> CliResult<PersistedPanelMembership> {
        let entry = self.manifest.filter.as_ref().ok_or_else(|| {
            stale(
                "persistent panel membership sidecar is absent from the search manifest; rebuild the exact panel generation before panel-scoped reads",
            )
        })?;
        let ids = filter::panel_membership(
            &self.vault_dir,
            entry,
            self.manifest.base_seq,
            self.manifest.panel_version,
        )?;
        Ok(PersistedPanelMembership {
            panel_version: self.manifest.panel_version,
            base_seq: self.manifest.base_seq,
            manifest_sha256: self.manifest_sha256.clone(),
            sidecar_sha256: entry.sha256.clone(),
            ids,
        })
    }

    pub(crate) fn filter_candidates_reconciled(
        &self,
        filters: &QueryFilters,
        changed: &BTreeSet<CxId>,
        replacements: &BTreeMap<CxId, Constellation>,
    ) -> CliResult<Option<BTreeSet<CxId>>> {
        let Some(mut candidates) = self.filter_candidates(filters)? else {
            return Ok(None);
        };
        candidates.retain(|cx_id| !changed.contains(cx_id));
        candidates.extend(
            replacements
                .values()
                .filter(|cx| filter::constellation_matches(cx, filters))
                .map(|cx| cx.cx_id),
        );
        Ok(Some(candidates))
    }

    pub fn max_len(&self) -> usize {
        self.max_len_for_slots(None)
    }

    pub fn base_seq(&self) -> u64 {
        self.manifest.base_seq
    }

    pub fn manifest_sha256(&self) -> &str {
        &self.manifest_sha256
    }

    /// Whether this handle reused the already-open immutable runtime for the
    /// exact `(vault, panel, manifest_sha256)` generation.
    pub fn runtime_cache_hit(&self) -> bool {
        self.runtime_cache_hit
    }

    pub fn max_len_for_slots(&self, allowed_slots: Option<&BTreeSet<SlotId>>) -> usize {
        self.manifest
            .slots
            .iter()
            .filter(|entry| {
                allowed_slots
                    .map(|allowed| allowed.contains(&SlotId::new(entry.slot)))
                    .unwrap_or(true)
            })
            .map(|entry| entry.len)
            .max()
            .unwrap_or(0)
    }

    pub fn ensure_search_bounded(&self) -> CliResult {
        self.ensure_search_bounded_for_slots(None)
    }

    pub fn ensure_search_bounded_for_slots(
        &self,
        allowed_slots: Option<&BTreeSet<SlotId>>,
    ) -> CliResult {
        for entry in &self.manifest.slots {
            if allowed_slots
                .map(|allowed| !allowed.contains(&SlotId::new(entry.slot)))
                .unwrap_or(false)
            {
                continue;
            }
            if entry.kind == "multi_maxsim" || entry.kind == "multi_maxsim_segments" {
                multi::ensure_bounded_sidecar(&self.vault_dir, entry, SlotId::new(entry.slot))?;
            }
        }
        Ok(())
    }

    fn require_entry(&self, slot: SlotId) -> CliResult<&SearchIndexEntry> {
        self.manifest
            .slots
            .iter()
            .find(|entry| entry.slot == slot.get())
            .ok_or_else(|| {
                stale(format!(
                    "persistent search manifest has no index for active slot {slot}; reingest or backfill the vault before search"
                ))
            })
    }
}

pub fn read_live_manifest_artifact(
    vault_dir: &Path,
    panel_version: u32,
) -> CliResult<PersistedSearchManifestArtifact> {
    PersistedSearchIndexes::open(vault_dir, panel_version)?.manifest_artifact()
}

pub fn read_candidate_manifest_artifact(
    vault_dir: &Path,
    panel_version: u32,
    candidate_key: [u8; 32],
    base_seq: u64,
) -> CliResult<PersistedSearchManifestArtifact> {
    PersistedSearchIndexes::open_candidate(vault_dir, panel_version, candidate_key, base_seq)?
        .manifest_artifact()
}

/// Reclaims one fully measured candidate after Anneal durably rejected it.
///
/// The exact candidate is reopened before deletion, the live manifest must be
/// a different immutable artifact, and its mmap runtime must have no remaining
/// borrowers. A live query therefore turns cleanup into a deferred outcome
/// rather than allowing Windows path deletion to race mapped search state.
pub fn retire_rejected_candidate_manifest_artifact(
    vault_dir: &Path,
    panel_version: u32,
    candidate_key: [u8; 32],
    artifact: &PersistedSearchManifestArtifact,
) -> CliResult<bool> {
    if artifact.panel_version != panel_version {
        return Err(stale(format!(
            "cannot retire rejected candidate for panel {panel_version}: artifact declares panel {}",
            artifact.panel_version
        )));
    }
    let reopened = read_candidate_manifest_artifact(
        vault_dir,
        panel_version,
        candidate_key,
        artifact.base_seq,
    )?;
    if reopened != *artifact {
        return Err(stale(format!(
            "rejected candidate readback differs from sealed artifact {}",
            artifact.manifest_sha256
        )));
    }
    let live_before = read_live_manifest_artifact(vault_dir, panel_version)?;
    if live_before == *artifact {
        return Err(stale(format!(
            "refusing to retire candidate {} because it is the live search manifest",
            artifact.manifest_sha256
        )));
    }
    if !release_cached_generation_runtime(vault_dir, panel_version, &artifact.manifest_sha256)? {
        return Ok(false);
    }
    let root = candidate_index_root(vault_dir, panel_version, candidate_key, artifact.base_seq);
    rebuild_stream::remove_failed_candidate_generation(&root)?;
    let live_after = read_live_manifest_artifact(vault_dir, panel_version)?;
    if live_after != live_before {
        return Err(stale(format!(
            "live search manifest changed from {} to {} across rejected-candidate cleanup",
            live_before.manifest_sha256, live_after.manifest_sha256
        )));
    }
    Ok(true)
}

fn release_cached_generation_runtime(
    vault_dir: &Path,
    panel_version: u32,
    manifest_sha256: &str,
) -> CliResult<bool> {
    let Some(cache) = OPEN_GENERATION_CACHE.get() else {
        return Ok(true);
    };
    let key = OpenGenerationKey {
        vault_dir: canonical_pin_vault_dir(vault_dir)?,
        panel_version,
        manifest_sha256: manifest_sha256.to_owned(),
    };
    let mut cache = cache.lock().map_err(|_| {
        CliError::io(
            "CALYX_SEARCH_OPEN_GENERATION_CACHE_POISONED: generation runtime cache lock was poisoned during rejected-candidate cleanup; remediation=restart the process and inspect the first panic",
        )
    })?;
    let released = match cache.entries.remove(&key) {
        Some(runtime) => {
            let weak = Arc::downgrade(&runtime);
            drop(runtime);
            weak.upgrade().is_none()
        }
        None => true,
    };
    cache.order.retain(|candidate| candidate != &key);
    cache.retired.retain(|runtime| runtime.strong_count() > 0);
    Ok(released)
}

/// Validates every artifact referenced by `artifact`, durably publishes its
/// manifest as live, then independently reopens the physical manifest.
pub fn publish_live_manifest_artifact(
    vault_dir: &Path,
    panel_version: u32,
    artifact: &PersistedSearchManifestArtifact,
) -> CliResult<PersistedSearchGeneration> {
    if artifact.panel_version != panel_version {
        return Err(stale(format!(
            "cannot publish manifest artifact for panel {} as panel {panel_version}",
            artifact.panel_version
        )));
    }
    if sha256_hex(&artifact.manifest_bytes) != artifact.manifest_sha256 {
        return Err(stale(format!(
            "manifest artifact bytes do not match declared SHA-256 {}",
            artifact.manifest_sha256
        )));
    }
    let indexes = PersistedSearchIndexes::from_manifest_bytes(
        vault_dir,
        panel_version,
        &artifact.manifest_bytes,
    )?;
    if indexes.manifest.base_seq != artifact.base_seq {
        return Err(stale(format!(
            "manifest artifact declares Base seq {}, expected {}",
            indexes.manifest.base_seq, artifact.base_seq
        )));
    }
    rebuild_stream::validate_staged_manifest_artifacts(vault_dir, &indexes.manifest)?;
    write_json_atomic_durable(&manifest_path(vault_dir, panel_version), &indexes.manifest)?;
    let reopened = PersistedSearchIndexes::open(vault_dir, panel_version)?;
    if reopened.manifest_sha256 != artifact.manifest_sha256 {
        return Err(stale(format!(
            "published live manifest readback hash {} differs from expected {}",
            reopened.manifest_sha256, artifact.manifest_sha256
        )));
    }
    reopened.generation()
}

pub fn validate_rebuild_config() -> CliResult {
    rebuild_plan::validate_parallel_rebuild_config()
}

impl SearchIndexEntry {
    pub(super) fn dense(
        slot: SlotId,
        dim: u32,
        len: usize,
        base_seq: u64,
        graph_rel: String,
        id_map_rel: String,
        dense_quantization: Option<DenseQuantizationEntry>,
    ) -> Self {
        Self {
            slot: slot.get(),
            kind: "diskann".to_string(),
            dim: Some(dim),
            token_dim: None,
            len,
            built_at_seq: base_seq,
            graph_rel: Some(graph_rel),
            id_map_rel: Some(id_map_rel),
            index_rel: None,
            sha256: None,
            token_count: None,
            dense_quantization: dense_quantization.map(Box::new),
        }
    }

    pub(super) fn flat_dense(
        slot: SlotId,
        dim: u32,
        len: usize,
        base_seq: u64,
        index_rel: String,
        sha256: String,
    ) -> Self {
        Self {
            slot: slot.get(),
            kind: "flat_dense".to_string(),
            dim: Some(dim),
            token_dim: None,
            len,
            built_at_seq: base_seq,
            graph_rel: None,
            id_map_rel: None,
            index_rel: Some(index_rel),
            sha256: Some(sha256),
            token_count: None,
            dense_quantization: None,
        }
    }

    pub(super) fn sparse(
        slot: SlotId,
        dim: u32,
        len: usize,
        base_seq: u64,
        index_rel: String,
        sha256: String,
        kind: &str,
    ) -> Self {
        Self {
            slot: slot.get(),
            kind: kind.to_string(),
            dim: Some(dim),
            token_dim: None,
            len,
            built_at_seq: base_seq,
            graph_rel: None,
            id_map_rel: None,
            index_rel: Some(index_rel),
            sha256: Some(sha256),
            token_count: None,
            dense_quantization: None,
        }
    }

    pub(super) fn multi_segments(
        slot: SlotId,
        token_dim: u32,
        len: usize,
        token_count: usize,
        base_seq: u64,
        index_rel: String,
        sha256: String,
    ) -> Self {
        Self {
            slot: slot.get(),
            kind: "multi_maxsim_segments".to_string(),
            dim: None,
            token_dim: Some(token_dim),
            len,
            built_at_seq: base_seq,
            graph_rel: None,
            id_map_rel: None,
            index_rel: Some(index_rel),
            sha256: Some(sha256),
            token_count: Some(token_count),
            dense_quantization: None,
        }
    }

    pub(super) fn require_kind(&self, expected: &str, slot: SlotId) -> CliResult {
        if self.kind == expected {
            return Ok(());
        }
        Err(stale(format!(
            "persistent slot {slot} index kind {} is not {expected}; rebuild the vault search indexes",
            self.kind
        )))
    }

    pub(super) fn require_dim(&self, slot: SlotId) -> CliResult<u32> {
        self.dim.ok_or_else(|| {
            stale(format!(
                "persistent slot {slot} manifest is missing dim; rebuild the vault search indexes"
            ))
        })
    }

    pub(super) fn require_token_dim(&self, slot: SlotId) -> CliResult<u32> {
        self.token_dim.ok_or_else(|| {
            stale(format!(
                "persistent slot {slot} manifest is missing token_dim; rebuild the vault search indexes"
            ))
        })
    }

    pub(super) fn require_graph_rel(&self, slot: SlotId) -> CliResult<&str> {
        self.graph_rel.as_deref().ok_or_else(|| {
            stale(format!(
                "persistent slot {slot} manifest is missing graph path; rebuild the vault search indexes"
            ))
        })
    }

    pub(super) fn require_id_map_rel(&self, slot: SlotId) -> CliResult<&str> {
        self.id_map_rel.as_deref().ok_or_else(|| {
            stale(format!(
                "persistent slot {slot} manifest is missing id map path; rebuild the vault search indexes"
            ))
        })
    }

    pub(super) fn require_index_rel(&self, slot: SlotId) -> CliResult<&str> {
        self.index_rel.as_deref().ok_or_else(|| {
            stale(format!(
                "persistent slot {slot} manifest is missing sidecar path; rebuild the vault search indexes"
            ))
        })
    }

    pub(super) fn require_sha256(&self, slot: SlotId) -> CliResult<&str> {
        self.sha256.as_deref().ok_or_else(|| {
            stale(format!(
                "persistent slot {slot} manifest is missing sidecar sha256; rebuild the vault search indexes"
            ))
        })
    }
}

fn panel_index_root(vault_dir: &Path, panel_version: u32) -> PathBuf {
    vault_dir
        .join(INDEX_ROOT)
        .join(format!("panel_{panel_version:010}"))
}

fn candidate_index_root(
    vault_dir: &Path,
    panel_version: u32,
    candidate_key: [u8; 32],
    base_seq: u64,
) -> PathBuf {
    panel_index_root(vault_dir, panel_version)
        .join("candidates")
        .join(hex32(candidate_key))
        .join(format!("base_{base_seq:020}"))
}

fn candidate_manifest_path(
    vault_dir: &Path,
    panel_version: u32,
    candidate_key: [u8; 32],
    base_seq: u64,
) -> PathBuf {
    candidate_index_root(vault_dir, panel_version, candidate_key, base_seq).join(MANIFEST_NAME)
}

fn hex32(hash: [u8; 32]) -> String {
    hash.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn legacy_manifest_path(vault_dir: &Path) -> PathBuf {
    vault_dir.join(INDEX_ROOT).join(MANIFEST_NAME)
}

/// Manifest path for one panel's persisted search generation.
///
/// Public so an operational surface can name the exact expected location even
/// when the file is absent (issue #1891) — an absent index must be reported with
/// the path an operator can go look at, not as silence.
#[must_use]
pub fn manifest_path(vault_dir: &Path, panel_version: u32) -> PathBuf {
    panel_index_root(vault_dir, panel_version).join(MANIFEST_NAME)
}

#[path = "persisted/io.rs"]
mod fs_io;
use fs_io::{
    HashingReader, rel, sha256_file, sha256_hex, stale, write_atomic_hashed, write_json_atomic,
    write_json_atomic_durable, write_json_atomic_hashed,
};
