//! Cold-path grounding-kernel maintenance and health (#1675).
//!
//! `intelligence.rs` can already select and persist one grounding kernel. What
//! was missing is the two things that make it an operable subsystem rather than
//! a callable function:
//!
//! 1. **A maintenance sweep.** Kernel selection is MFVS over an all-pairs kNN
//!    graph plus a full-corpus recall measurement. That is minutes of CPU, not
//!    microseconds, so it belongs on a COLD path — never the ingest hot path.
//!    Every entry point here calls [`assert_cold_calyx`] so a hot-context caller
//!    trips the #1686 boundary instrument instead of silently stalling ingest.
//!    The sweep scopes per domain: one kernel per grounded outcome axis present
//!    in the panel (calyx-lodestar `Scope::Domain { anchor_kind }` semantics),
//!    because a single kernel blended over unrelated outcome axes is not the
//!    minimal generating core of any of them.
//! 2. **A health surface.** `calyx_lodestar::kernel_health` is defined to READ
//!    the persisted Kernel artifact and never recompute recall or groundedness.
//!    It needs a [`KernelArtifactStore`]; the only Source of Truth Synapse has
//!    is the vault, so [`VaultKernelArtifactStore`] is that store, keyed by
//!    kernel id inside the native `Kernel` CF.
//!
//! Why the sweep is not routed through `calyx_lodestar::multi_scope::build_kernel`:
//! that entry point materializes its graph from an [`AssocStore`] over a
//! persisted association graph. Synapse has no persisted association graph — the
//! kernel graph is derived on demand from the panel's dense lens vectors by
//! Forge kNN. Implementing `AssocStore` would mean materializing that same graph
//! a second time purely to satisfy the trait. The per-domain *scope* is what
//! matters and it is preserved exactly (`Scope::Domain { anchor_kind }`), while
//! the graph keeps its single authoritative construction.
//!
//! [`AssocStore`]: calyx_lodestar::AssocStore
//! [`assert_cold_calyx`]: crate::lowering::hot_context::assert_cold_calyx

use std::collections::BTreeMap;

use calyx_aster::cf::ColumnFamily;
use calyx_aster::vault::encode::decode_constellation_base;
use calyx_core::CxId;
use calyx_lodestar::{
    KernelArtifactStore, KernelHealth, KernelTrust, LodestarError, RecallPassMode,
    kernel_health_from_kernel, read_kernel_artifact,
};
use serde::{Deserialize, Serialize};

use crate::intelligence::{KERNEL_ROW_PREFIX, kernel_row_key};
use crate::{SynapseCalyxCfWrite, SynapseCalyxError, SynapseCalyxKernelParams, SynapseCalyxVault};

/// Row prefix of a persisted full Kernel artifact inside the `Kernel` CF.
const KERNEL_ARTIFACT_PREFIX: &[u8; 5] = b"KART1";
/// Row prefix of the per-domain kernel index inside the `Kernel` CF.
const KERNEL_DOMAIN_PREFIX: &[u8; 5] = b"KDOM1";
/// Upper bound on domains rebuilt in one sweep. A sweep is minutes of CPU per
/// domain; an unbounded sweep would be an unbounded maintenance window.
pub const SYNAPSE_KERNEL_MAX_DOMAINS: usize = 64;

/// The vault-backed [`KernelArtifactStore`].
///
/// The full Kernel artifact lives in the native `Kernel` CF keyed by its own
/// kernel id, so kernel health is a physical readback of what was selected
/// rather than a recomputation.
pub struct VaultKernelArtifactStore<'vault> {
    vault: &'vault SynapseCalyxVault,
}

impl<'vault> VaultKernelArtifactStore<'vault> {
    #[must_use]
    pub const fn new(vault: &'vault SynapseCalyxVault) -> Self {
        Self { vault }
    }
}

impl KernelArtifactStore for VaultKernelArtifactStore<'_> {
    fn write_kernel_bytes(&self, kernel_id: CxId, bytes: &[u8]) -> Result<(), LodestarError> {
        self.vault
            .write_cf_batch(vec![SynapseCalyxCfWrite {
                cf: ColumnFamily::Kernel,
                key: artifact_key(kernel_id),
                value: bytes.to_vec(),
            }])
            .map_err(|error| LodestarError::KernelIndexIo {
                detail: format!("write kernel artifact row to the Kernel CF: {error}"),
            })?;
        self.vault
            .flush()
            .map_err(|error| LodestarError::KernelIndexIo {
                detail: format!("flush the Kernel CF after the artifact write: {error}"),
            })
    }

    fn read_kernel_bytes(&self, kernel_id: CxId) -> Result<Option<Vec<u8>>, LodestarError> {
        self.vault
            .read_cf_latest(ColumnFamily::Kernel, &artifact_key(kernel_id))
            .map_err(|error| LodestarError::KernelIndexIo {
                detail: format!("read kernel artifact row from the Kernel CF: {error}"),
            })
    }
}

/// Bounded request for one cold per-domain kernel rebuild sweep.
#[derive(Clone, Debug)]
pub struct SynapseCalyxKernelRebuildParams {
    pub panel_version: u32,
    pub content_slot: u16,
    pub max_records: usize,
    pub knn: usize,
    pub edge_cos_threshold: f32,
    pub min_recall_ratio: f32,
    pub max_domains: usize,
}

impl SynapseCalyxKernelRebuildParams {
    #[must_use]
    pub const fn new(panel_version: u32, content_slot: u16) -> Self {
        Self {
            panel_version,
            content_slot,
            max_records: crate::SYNAPSE_INTELLIGENCE_MAX_RECORDS,
            knn: crate::SYNAPSE_KERNEL_DEFAULT_KNN,
            edge_cos_threshold: crate::SYNAPSE_KERNEL_DEFAULT_EDGE_COS,
            min_recall_ratio: crate::SYNAPSE_KERNEL_DEFAULT_MIN_RECALL,
            max_domains: SYNAPSE_KERNEL_MAX_DOMAINS,
        }
    }

    const fn kernel_params(&self, anchor_kind: Option<String>) -> SynapseCalyxKernelParams {
        SynapseCalyxKernelParams {
            panel_version: self.panel_version,
            content_slot: self.content_slot,
            max_records: self.max_records,
            knn: self.knn,
            edge_cos_threshold: self.edge_cos_threshold,
            min_recall_ratio: self.min_recall_ratio,
            anchor_kind,
        }
    }
}

/// One domain's outcome in a rebuild sweep. A refusal is reported with its
/// structured code and never overwritten by a weaker kernel.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SynapseCalyxKernelDomainOutcome {
    pub anchor_kind: String,
    pub anchored_records: usize,
    pub built: bool,
    pub kernel_id: Option<String>,
    pub members: usize,
    pub corpus_size: usize,
    pub recall_kernel_only: f32,
    pub recall_ratio: f32,
    pub reached_anchor: f32,
    pub refusal_code: Option<String>,
    pub refusal: Option<String>,
}

/// Result of one cold per-domain kernel rebuild sweep, with the physical
/// `Kernel` CF readback.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SynapseCalyxKernelRebuildReport {
    pub panel_version: u32,
    pub content_slot: u16,
    pub domains_discovered: usize,
    pub domains_attempted: usize,
    pub domains_built: usize,
    pub domains_refused: usize,
    pub all_domains_grounded: bool,
    pub min_recall_ratio: f32,
    pub domains: Vec<SynapseCalyxKernelDomainOutcome>,
    pub artifacts_persisted: usize,
    pub kernel_cf_rows_after: usize,
}

/// Kernel health assembled from the persisted artifact — never recomputed.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SynapseCalyxKernelHealthReport {
    pub panel_version: u32,
    pub content_slot: u16,
    pub anchor_kind: Option<String>,
    pub kernel_id: String,
    pub size: usize,
    pub kernel_graph_size: usize,
    pub recall_raw: f32,
    pub recall_ratio: f32,
    pub min_recall_ratio: f32,
    pub n_queries_tested: usize,
    pub recall_pass_mode: String,
    pub grounded_fraction: f32,
    pub unanchored_count: usize,
    pub approx_factor: f64,
    pub tau_star_estimate: usize,
    pub tau_star_exact: bool,
    pub built_at_millis: u64,
    pub corpus_shard_hash: String,
    pub trust: String,
    pub warnings: Vec<String>,
    pub artifact_bytes: usize,
    pub kernel_cf_rows: usize,
}

impl SynapseCalyxVault {
    /// Rebuilds one grounding kernel per grounded outcome domain in a panel,
    /// persisting each full Kernel artifact plus a per-domain index row to the
    /// native `Kernel` CF, then reading the CF back.
    ///
    /// This is a COLD maintenance pass by construction: it scans the whole
    /// panel, runs Forge kNN over every embedded concept, selects an MFVS
    /// kernel and measures recall against the full corpus, once per domain.
    ///
    /// Refusals are per domain and loud: a domain whose kernel does not meet
    /// the recall gate (or has no anchor, or too few embedded concepts) is
    /// reported with its structured code and nothing is persisted for it. The
    /// sweep as a whole fails closed when it could not build a single kernel —
    /// an empty sweep reported as success would read as "kernels are healthy".
    ///
    /// # Errors
    ///
    /// Returns a structured error when the panel has no grounded outcome domain
    /// at all, when every discovered domain refused, or when a CF scan fails.
    pub fn rebuild_domain_kernels(
        &self,
        params: &SynapseCalyxKernelRebuildParams,
    ) -> Result<SynapseCalyxKernelRebuildReport, SynapseCalyxError> {
        crate::lowering::hot_context::assert_cold_calyx("rebuild_domain_kernels");
        let discovered = self.discover_panel_domains(params.panel_version, params.max_records)?;
        if discovered.is_empty() {
            return Err(SynapseCalyxError::new(
                "SYNAPSE_CALYX_KERNEL_NO_DOMAIN",
                format!(
                    "panel {} has no grounded outcome domain (no constellation carries an anchor with confidence > 0); there is nothing to select a kernel over",
                    params.panel_version
                ),
                "anchor at least one concept with a grounded outcome (AnchorKind + confidence > 0) in this panel, then rerun the kernel rebuild",
            ));
        }
        let domains_discovered = discovered.len();
        let max_domains = params.max_domains.clamp(1, SYNAPSE_KERNEL_MAX_DOMAINS);

        let mut outcomes = Vec::new();
        let mut artifacts_persisted = 0usize;
        for (anchor_kind, anchored_records) in discovered.into_iter().take(max_domains) {
            let scoped = params.kernel_params(Some(anchor_kind.clone()));
            match self.build_domain_kernel(&scoped) {
                Ok(report) => {
                    self.persist_domain_index_row(&anchor_kind, &scoped, &report)?;
                    artifacts_persisted += 1;
                    outcomes.push(SynapseCalyxKernelDomainOutcome {
                        anchor_kind,
                        anchored_records,
                        built: true,
                        kernel_id: Some(report.kernel_id),
                        members: report.members,
                        corpus_size: report.corpus_size,
                        recall_kernel_only: report.recall_kernel_only,
                        recall_ratio: report.recall_ratio,
                        reached_anchor: report.reached_anchor,
                        refusal_code: None,
                        refusal: None,
                    });
                }
                Err(error) => outcomes.push(SynapseCalyxKernelDomainOutcome {
                    anchor_kind,
                    anchored_records,
                    built: false,
                    kernel_id: None,
                    members: 0,
                    corpus_size: 0,
                    recall_kernel_only: 0.0,
                    recall_ratio: 0.0,
                    reached_anchor: 0.0,
                    refusal_code: Some(error.code.to_owned()),
                    refusal: Some(error.to_string()),
                }),
            }
        }

        let domains_attempted = outcomes.len();
        let domains_built = outcomes.iter().filter(|outcome| outcome.built).count();
        let domains_refused = domains_attempted - domains_built;
        if domains_built == 0 {
            let first = outcomes
                .iter()
                .find_map(|outcome| outcome.refusal.clone())
                .unwrap_or_else(|| "no domain produced a refusal reason".to_owned());
            return Err(SynapseCalyxError::new(
                "SYNAPSE_CALYX_KERNEL_REBUILD_ALL_DOMAINS_REFUSED",
                format!(
                    "kernel rebuild attempted {domains_attempted} domain(s) in panel {} slot {} and every one refused; first refusal: {first}",
                    params.panel_version, params.content_slot
                ),
                "repair the named per-domain refusal (widen the corpus, embed the content slot, or anchor concepts) — an empty kernel sweep is never reported as healthy",
            ));
        }

        let kernel_cf_rows_after = self.count_cf_latest(ColumnFamily::Kernel)?;
        Ok(SynapseCalyxKernelRebuildReport {
            panel_version: params.panel_version,
            content_slot: params.content_slot,
            domains_discovered,
            domains_attempted,
            domains_built,
            domains_refused,
            all_domains_grounded: domains_refused == 0 && domains_attempted == domains_discovered,
            min_recall_ratio: params.min_recall_ratio,
            domains: outcomes,
            artifacts_persisted,
            kernel_cf_rows_after,
        })
    }

    /// Reports the health of one persisted domain kernel by READING its Kernel
    /// artifact from the `Kernel` CF (`calyx_lodestar::kernel_health` semantics:
    /// recall and groundedness are reported exactly as persisted, never
    /// re-measured).
    ///
    /// # Errors
    ///
    /// Returns a structured error when no kernel has been persisted for the
    /// requested domain, when the artifact is missing/stale/undecodable, or
    /// when a CF read fails.
    pub fn domain_kernel_health(
        &self,
        panel_version: u32,
        content_slot: u16,
        anchor_kind: Option<&str>,
    ) -> Result<SynapseCalyxKernelHealthReport, SynapseCalyxError> {
        crate::lowering::hot_context::assert_cold_calyx("domain_kernel_health");
        let index_key = anchor_kind.map_or_else(
            || kernel_row_key(panel_version, content_slot),
            |kind| domain_index_key(panel_version, content_slot, kind),
        );
        let Some(index_bytes) = self.read_cf_latest(ColumnFamily::Kernel, &index_key)? else {
            return Err(SynapseCalyxError::new(
                "SYNAPSE_CALYX_KERNEL_NOT_BUILT",
                format!(
                    "no grounding kernel is persisted for panel {panel_version} slot {content_slot} domain {}; the Kernel CF has no row at this key",
                    anchor_kind.unwrap_or("<panel default>")
                ),
                "run the cold kernel rebuild (hygiene kernel_rebuild) for this panel before asking for kernel health; health never re-derives a kernel it cannot read",
            ));
        };
        let index: serde_json::Value = serde_json::from_slice(&index_bytes).map_err(|error| {
            SynapseCalyxError::new(
                "SYNAPSE_CALYX_KERNEL_INDEX_DECODE_FAILED",
                format!("decode the persisted kernel index row: {error}"),
                "the Kernel CF index row is corrupt; rebuild the domain kernel",
            )
        })?;
        let kernel_id_text = index
            .get("kernel_id")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                SynapseCalyxError::new(
                    "SYNAPSE_CALYX_KERNEL_INDEX_DECODE_FAILED",
                    "the persisted kernel index row carries no kernel_id".to_owned(),
                    "the Kernel CF index row is corrupt; rebuild the domain kernel",
                )
            })?;
        let kernel_id = crate::parse_cx_id(kernel_id_text)?;

        let store = VaultKernelArtifactStore::new(self);
        let artifact_bytes = store
            .read_kernel_bytes(kernel_id)
            .map_err(|error| {
                crate::intelligence::kernel_math_error("read kernel artifact", &error)
            })?
            .map_or(0, |bytes| bytes.len());
        let kernel = read_kernel_artifact(kernel_id, &store).map_err(|error| {
            crate::intelligence::kernel_math_error("read the persisted Kernel artifact", &error)
        })?;
        let health: KernelHealth = kernel_health_from_kernel(&kernel);
        let kernel_cf_rows = self.count_cf_latest(ColumnFamily::Kernel)?;

        Ok(SynapseCalyxKernelHealthReport {
            panel_version,
            content_slot,
            anchor_kind: anchor_kind.map(str::to_owned),
            kernel_id: health.kernel_id.to_string(),
            size: health.size,
            kernel_graph_size: health.kernel_graph_size,
            recall_raw: health.recall.raw,
            recall_ratio: health.recall.ratio,
            min_recall_ratio: health.recall.min_recall_ratio,
            n_queries_tested: health.recall.n_queries_tested,
            recall_pass_mode: recall_pass_mode_label(health.recall.pass_mode).to_owned(),
            grounded_fraction: health.grounded_fraction,
            unanchored_count: health.unanchored_count,
            approx_factor: health.approx_factor,
            tau_star_estimate: health.tau_star_estimate,
            tau_star_exact: health.tau_star_exact,
            built_at_millis: health.built_at_millis,
            corpus_shard_hash: health.corpus_shard_hash,
            trust: trust_label(health.trust).to_owned(),
            warnings: health.warnings,
            artifact_bytes,
            kernel_cf_rows,
        })
    }

    /// Lists every grounded outcome domain present in a panel with its anchored
    /// record count, most-anchored domain first (ties broken by label so a
    /// sweep is deterministic).
    fn discover_panel_domains(
        &self,
        panel_version: u32,
        max_records: usize,
    ) -> Result<Vec<(String, usize)>, SynapseCalyxError> {
        let max_records = max_records.clamp(1, crate::SYNAPSE_INTELLIGENCE_MAX_RECORDS);
        let mut counts: BTreeMap<String, usize> = BTreeMap::new();
        let mut scanned = 0usize;
        for (_, value) in self.scan_cf_latest(ColumnFamily::Base)? {
            let constellation = decode_constellation_base(&value).map_err(|error| {
                SynapseCalyxError::from_calyx("decode Base constellation", &error)
            })?;
            if constellation.panel_version != panel_version {
                continue;
            }
            scanned += 1;
            for anchor in &constellation.anchors {
                if anchor.confidence > 0.0 {
                    *counts
                        .entry(crate::grounding::anchor_kind_label(&anchor.kind))
                        .or_default() += 1;
                }
            }
            if scanned >= max_records {
                break;
            }
        }
        let mut domains: Vec<(String, usize)> = counts.into_iter().collect();
        domains.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
        Ok(domains)
    }

    /// Persists the per-domain kernel index row that `domain_kernel_health`
    /// resolves a kernel id through.
    fn persist_domain_index_row(
        &self,
        anchor_kind: &str,
        params: &SynapseCalyxKernelParams,
        report: &crate::SynapseCalyxKernelReport,
    ) -> Result<(), SynapseCalyxError> {
        let row = serde_json::json!({
            "panel_version": params.panel_version,
            "content_slot": params.content_slot,
            "anchor_kind": anchor_kind,
            "kernel_id": report.kernel_id,
            "corpus_fingerprint": report.corpus_fingerprint,
            "members": report.members,
            "corpus_size": report.corpus_size,
            "recall_kernel_only": report.recall_kernel_only,
            "recall_ratio": report.recall_ratio,
            "min_recall_ratio": report.min_recall_ratio,
            "reached_anchor": report.reached_anchor,
        });
        let value = serde_json::to_vec(&row).map_err(|error| {
            SynapseCalyxError::new(
                "SYNAPSE_CALYX_KERNEL_INDEX_ENCODE_FAILED",
                format!("encode the per-domain kernel index row: {error}"),
                "inspect the kernel report fields before retrying the rebuild",
            )
        })?;
        self.write_cf_batch(vec![SynapseCalyxCfWrite {
            cf: ColumnFamily::Kernel,
            key: domain_index_key(params.panel_version, params.content_slot, anchor_kind),
            value,
        }])?;
        self.flush()
    }
}

fn artifact_key(kernel_id: CxId) -> Vec<u8> {
    let mut key = Vec::with_capacity(KERNEL_ARTIFACT_PREFIX.len() + 16);
    key.extend_from_slice(KERNEL_ARTIFACT_PREFIX);
    key.extend_from_slice(&kernel_id.to_bytes());
    key
}

fn domain_index_key(panel_version: u32, content_slot: u16, anchor_kind: &str) -> Vec<u8> {
    let mut key = Vec::with_capacity(KERNEL_DOMAIN_PREFIX.len() + 6 + anchor_kind.len());
    key.extend_from_slice(KERNEL_DOMAIN_PREFIX);
    key.extend_from_slice(&panel_version.to_be_bytes());
    key.extend_from_slice(&content_slot.to_be_bytes());
    key.extend_from_slice(anchor_kind.as_bytes());
    debug_assert_ne!(
        &key[..KERNEL_DOMAIN_PREFIX.len()],
        &KERNEL_ROW_PREFIX[..],
        "the per-domain index prefix must not collide with the kernel report prefix"
    );
    key
}

const fn recall_pass_mode_label(mode: RecallPassMode) -> &'static str {
    match mode {
        RecallPassMode::Untested => "untested",
        RecallPassMode::Passed => "passed",
        RecallPassMode::BelowGate => "below_gate",
    }
}

const fn trust_label(trust: KernelTrust) -> &'static str {
    match trust {
        KernelTrust::Anchored => "anchored",
        KernelTrust::Provisional => "provisional",
        KernelTrust::Empty => "empty",
    }
}
