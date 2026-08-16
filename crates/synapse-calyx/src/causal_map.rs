//! Exhaustive, typed causal-evidence maps over temporal Calyx constellations.
//!
//! This module deliberately does not manufacture one universal "causal score".
//! Every estimator answers a different question under different assumptions, so
//! the persisted artifact keeps conditional association, directed predictive
//! information, nonlinear state-space evidence, event co-intensity, and Hawkes
//! triggering in separate lanes. Observational estimator success never upgrades
//! the artifact to an identified structural causal effect.

use std::collections::{BTreeMap, BTreeSet};

use calyx_assay::{
    CcmConfig, HawkesConfig, HawkesEventSeries, PartialNetworkSeries, PcSeries,
    cross_correlation_profile, exponential_hawkes_em, granger_causality_lags,
    partial_correlation_network, pc_stable_gaussian, temporal_cross_k, transfer_entropy_sweep,
};
use calyx_aster::cf::ColumnFamily;
use calyx_core::FixedClock;
use num_traits::ToPrimitive;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::intelligence::{
    SYNAPSE_TEMPORAL_MAX_BINS, SYNAPSE_TEMPORAL_MAX_LAGS, SynapseCalyxTemporalParams,
};
use crate::{SynapseCalyxCfWrite, SynapseCalyxError, SynapseCalyxVault};

/// Maximum stream count for one exhaustive map. The request fails rather than
/// sampling when the complete `C(n,2)` family cannot fit this declared budget.
pub const SYNAPSE_CAUSAL_MAP_MAX_STREAMS: usize = 16;
/// Default false-discovery-rate threshold for p-value families.
pub const SYNAPSE_CAUSAL_MAP_DEFAULT_FDR_ALPHA: f32 = 0.05;
/// Gaussian PC-stable conditioning depth. Calyx's present implementation is a
/// skeleton estimator; this bounded depth is explicit in every artifact.
pub const SYNAPSE_CAUSAL_MAP_PC_MAX_CONDITIONING: usize = 3;

const GRAPH_CAUSAL_MAP_PREFIX: &[u8; 5] = b"GCMP1";
const GRAPH_CAUSAL_MAP_INDEX_PREFIX: &[u8; 5] = b"GCMI1";
const CAUSAL_MAP_ARTIFACT_SCHEMA: &str = "synapse.calyx.causal_map.v2";
const CAUSAL_MAP_POINTER_SCHEMA: &str = "synapse.calyx.causal_map_pointer.v1";

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SynapseCalyxCausalEstimatorError {
    pub code: String,
    pub message: String,
    pub remediation: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SynapseCalyxCausalEstimatorEvidence {
    pub estimator: String,
    pub status: String,
    pub assumptions: Vec<String>,
    pub result: Option<Value>,
    pub error: Option<SynapseCalyxCausalEstimatorError>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SynapseCalyxCausalStream {
    pub name: String,
    pub event_count: usize,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SynapseCalyxCausalPairEvidence {
    pub group_a: String,
    pub group_b: String,
    pub events_a: usize,
    pub events_b: usize,
    pub transfer_entropy: SynapseCalyxCausalEstimatorEvidence,
    pub granger_a_to_b: SynapseCalyxCausalEstimatorEvidence,
    pub granger_b_to_a: SynapseCalyxCausalEstimatorEvidence,
    pub cross_correlation: SynapseCalyxCausalEstimatorEvidence,
    pub convergent_cross_mapping: SynapseCalyxCausalEstimatorEvidence,
    pub temporal_cross_k: SynapseCalyxCausalEstimatorEvidence,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SynapseCalyxFdrDecision {
    pub hypothesis: String,
    pub p_value: f32,
    pub q_value: f32,
    pub significant: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SynapseCalyxFdrFamily {
    pub name: String,
    pub method: String,
    /// Statistical scope in which the adjusted decisions control FDR. BH is
    /// not silently presented as arbitrary-dependence or cross-family control.
    pub assumptions: Vec<String>,
    pub alpha: f32,
    pub hypotheses_tested: usize,
    pub decisions: Vec<SynapseCalyxFdrDecision>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SynapseCalyxCausalMapArtifact {
    pub schema: String,
    pub panel_version: u32,
    pub group_key: String,
    pub pair_scope: String,
    pub since_ts_ns: Option<i64>,
    pub until_ts_ns: Option<i64>,
    pub max_records: usize,
    pub source_records: usize,
    /// SHA-256 over every sorted `(source_event_ns, group_value)` member in
    /// this exact requested population. A reader re-derives it before serving
    /// the materialized result, so a late backfill cannot make an old causal
    /// artifact look current.
    pub source_fingerprint_sha256: String,
    pub earliest_event_ns: u64,
    pub latest_event_ns: u64,
    pub bin_seconds: f64,
    pub n_bins: usize,
    pub max_lag: usize,
    pub fdr_alpha: f32,
    pub all_requested_records_loaded: bool,
    pub all_stream_pairs_enumerated: bool,
    pub evidence_class: String,
    pub structural_effect_identified: bool,
    pub structural_identification_reason: String,
    pub identification_requirements: Vec<String>,
    pub streams: Vec<SynapseCalyxCausalStream>,
    pub expected_pair_count: usize,
    pub pairs: Vec<SynapseCalyxCausalPairEvidence>,
    pub pc_stable_skeleton: SynapseCalyxCausalEstimatorEvidence,
    pub partial_correlation_network: SynapseCalyxCausalEstimatorEvidence,
    pub hawkes_branching_graph: SynapseCalyxCausalEstimatorEvidence,
    pub fdr_families: Vec<SynapseCalyxFdrFamily>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SynapseCalyxCausalMapReport {
    pub artifact: SynapseCalyxCausalMapArtifact,
    pub graph_key_hex: String,
    pub graph_value_sha256: String,
    pub graph_value_bytes: usize,
    pub pointer_key_hex: String,
    pub pointer_value_sha256: String,
    pub pointer_value_bytes: usize,
    pub graph_cf_rows_after: usize,
    pub physical_readback_matches: bool,
    pub pointer_readback_matches: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct SynapseCalyxCausalMapPointer {
    schema: String,
    scope_sha256: String,
    panel_version: u32,
    group_key: String,
    pair_scope: String,
    group_a: Option<String>,
    group_b: Option<String>,
    window_kind: String,
    window_span_ns: Option<i64>,
    bin_seconds_bits: u64,
    max_lag: usize,
    fdr_alpha_bits: u32,
    artifact_key_hex: String,
    artifact_sha256: String,
    source_fingerprint_sha256: String,
    source_records: usize,
    earliest_event_ns: u64,
    latest_event_ns: u64,
    since_ts_ns: Option<i64>,
    until_ts_ns: Option<i64>,
}

#[derive(Clone, Debug)]
struct CausalMapScope {
    scope_sha256: String,
    panel_version: u32,
    group_key: String,
    pair_scope: String,
    group_a: Option<String>,
    group_b: Option<String>,
    window_kind: String,
    window_span_ns: Option<i64>,
    bin_seconds: f64,
    max_lag: usize,
    fdr_alpha: f32,
}

#[derive(Clone)]
struct Hypothesis {
    id: String,
    p_value: f32,
}

impl SynapseCalyxVault {
    /// Computes every stream pair in the requested temporal scope, persists one
    /// content-addressed Graph-CF artifact, and byte-compares a separate physical
    /// read before returning.
    ///
    /// # Errors
    ///
    /// Fails closed for an invalid window/FDR, missing group values, an
    /// incomplete one-sided pair scope, too many streams, over-limit source
    /// coverage, an empty/one-stream scope, persistence failure, or readback
    /// mismatch. Individual statistical lanes retain typed estimator errors in
    /// the complete artifact because non-applicability is itself causal evidence;
    /// it is never silently replaced by another estimator.
    #[allow(clippy::too_many_lines)]
    pub fn temporal_causal_map(
        &self,
        params: &SynapseCalyxTemporalParams,
        fdr_alpha: f32,
    ) -> Result<SynapseCalyxCausalMapReport, SynapseCalyxError> {
        crate::lowering::hot_context::assert_cold_calyx("temporal_causal_map");
        let scope = causal_map_scope(params, fdr_alpha)?;
        let group_key = scope.group_key.as_str();
        let pair_scope = scope.group_a.clone().zip(scope.group_b.clone());
        tracing::info!(
            code = "SYNAPSE_CALYX_CAUSAL_MAP_STARTED",
            panel_version = params.panel_version,
            group_key,
            since_ts_ns = params.since_ts_ns,
            until_ts_ns = params.until_ts_ns,
            max_records = params.max_records,
            fdr_alpha,
            "starting exhaustive typed causal-evidence map"
        );

        let records = self.load_panel_event_records(params, Some(group_key))?;
        if records.is_empty() {
            return Err(causal_error(
                "SYNAPSE_CALYX_CAUSAL_MAP_EMPTY_SCOPE",
                "the requested source-event-time scope contains no temporally active panel records",
                "confirm panel_version and group_key, then widen since_ts_ns/until_ts_ns to a scope containing real events",
            ));
        }
        let missing_groups = records
            .iter()
            .filter(|record| record.group.is_none())
            .count();
        if missing_groups != 0 {
            return Err(causal_error(
                "SYNAPSE_CALYX_CAUSAL_MAP_GROUP_VALUE_MISSING",
                format!(
                    "{missing_groups} of {} in-scope temporal records have no value for group_key={group_key}; omitting them would make the causal map incomplete",
                    records.len()
                ),
                "choose a group_key present on every in-scope record or narrow the source-event-time window to one homogeneous, fully classified population",
            ));
        }

        let mut by_group: BTreeMap<String, Vec<f64>> = BTreeMap::new();
        for record in &records {
            let Some(group) = record.group.as_ref() else {
                return Err(causal_error(
                    "SYNAPSE_CALYX_CAUSAL_MAP_GROUP_VALUE_MISSING",
                    "an in-scope record lost its group value after the complete missing-value check",
                    "inspect the immutable temporal record projection; group values must not change during one causal-map call",
                ));
            };
            by_group.entry(group.clone()).or_default().push(record.secs);
        }
        let selected = select_streams(&by_group, pair_scope.as_ref())?;
        if selected.len() > SYNAPSE_CAUSAL_MAP_MAX_STREAMS {
            return Err(causal_error(
                "SYNAPSE_CALYX_CAUSAL_MAP_STREAM_LIMIT_EXCEEDED",
                format!(
                    "the complete scope contains {} streams, exceeding the exhaustive-map limit {SYNAPSE_CAUSAL_MAP_MAX_STREAMS}; sampling streams is forbidden",
                    selected.len()
                ),
                "supply both group_a and group_b for an explicit pair or narrow the source-event-time scope so every stream pair fits the declared exhaustive budget",
            ));
        }
        let bin_seconds = scope.bin_seconds;
        let max_lag = scope.max_lag;
        let (origin, n_bins, binned) = aligned_binned_streams(&selected, &by_group, bin_seconds)?;
        let lags = (1..=max_lag).collect::<Vec<_>>();
        let mut granger_hypotheses = Vec::new();
        let mut ccf_hypotheses = Vec::new();
        let mut pairs = Vec::new();

        for left in 0..selected.len() {
            for right in (left + 1)..selected.len() {
                let group_a = &selected[left];
                let group_b = &selected[right];
                let stream_a = &binned[group_a];
                let stream_b = &binned[group_b];
                let times_a = &by_group[group_a];
                let times_b = &by_group[group_b];
                let transfer_entropy = transfer_entropy_evidence(stream_a, stream_b, &lags)?;
                let granger_a_to_b = granger_evidence(
                    group_a,
                    group_b,
                    stream_a,
                    stream_b,
                    &lags,
                    &mut granger_hypotheses,
                );
                let granger_b_to_a = granger_evidence(
                    group_b,
                    group_a,
                    stream_b,
                    stream_a,
                    &lags,
                    &mut granger_hypotheses,
                );
                let cross_correlation = cross_correlation_evidence(
                    group_a,
                    group_b,
                    stream_a,
                    stream_b,
                    max_lag,
                    &mut ccf_hypotheses,
                );
                let convergent_cross_mapping = ccm_evidence(group_a, group_b, stream_a, stream_b);
                let temporal_cross_k = cross_k_evidence(
                    group_a,
                    group_b,
                    times_a,
                    times_b,
                    origin,
                    bin_seconds,
                    max_lag,
                );
                pairs.push(SynapseCalyxCausalPairEvidence {
                    group_a: group_a.clone(),
                    group_b: group_b.clone(),
                    events_a: times_a.len(),
                    events_b: times_b.len(),
                    transfer_entropy,
                    granger_a_to_b,
                    granger_b_to_a,
                    cross_correlation,
                    convergent_cross_mapping,
                    temporal_cross_k,
                });
            }
        }

        let (pc_stable_skeleton, pc_hypotheses) = pc_evidence(&selected, &binned, fdr_alpha);
        let (partial_correlation_network, partial_hypotheses) =
            partial_network_evidence(&selected, &binned, fdr_alpha);
        let hawkes_branching_graph = hawkes_evidence(&selected, &by_group, origin, bin_seconds);
        let fdr_families = vec![
            fdr_family(
                "granger_all_pairs_directions_lags",
                fdr_alpha,
                granger_hypotheses,
            )?,
            fdr_family(
                "cross_correlation_all_pairs_lags",
                fdr_alpha,
                ccf_hypotheses,
            )?,
            fdr_family("pc_stable_removed_edge_tests", fdr_alpha, pc_hypotheses)?,
            fdr_family(
                "partial_correlation_all_pairs",
                fdr_alpha,
                partial_hypotheses,
            )?,
        ];
        let earliest_event_ns =
            records
                .iter()
                .map(|record| record.nanos)
                .min()
                .ok_or_else(|| {
                    causal_error(
                        "SYNAPSE_CALYX_CAUSAL_MAP_EMPTY_SCOPE",
                        "the causal-map scope lost every event before persistence",
                        "inspect the temporal record loader and source-event window",
                    )
                })?;
        let latest_event_ns = records
            .iter()
            .map(|record| record.nanos)
            .max()
            .unwrap_or(earliest_event_ns);
        let streams = selected
            .iter()
            .map(|name| SynapseCalyxCausalStream {
                name: name.clone(),
                event_count: by_group[name].len(),
            })
            .collect::<Vec<_>>();
        let expected_pair_count = selected.len() * selected.len().saturating_sub(1) / 2;
        let source_fingerprint_sha256 = source_fingerprint(&records)?;
        let artifact = SynapseCalyxCausalMapArtifact {
            schema: CAUSAL_MAP_ARTIFACT_SCHEMA.to_owned(),
            panel_version: params.panel_version,
            group_key: group_key.to_owned(),
            pair_scope: if pair_scope.is_some() { "explicit_pair" } else { "complete" }.to_owned(),
            since_ts_ns: params.since_ts_ns,
            until_ts_ns: params.until_ts_ns,
            max_records: params.max_records,
            source_records: records.len(),
            source_fingerprint_sha256,
            earliest_event_ns,
            latest_event_ns,
            bin_seconds,
            n_bins,
            max_lag,
            fdr_alpha,
            all_requested_records_loaded: true,
            all_stream_pairs_enumerated: pairs.len() == expected_pair_count,
            evidence_class: "observational_predictive".to_owned(),
            structural_effect_identified: false,
            structural_identification_reason: "the source is observational event timing; estimator agreement does not establish an intervention contrast or eliminate confounding".to_owned(),
            identification_requirements: vec![
                "persisted intervention/exposure definition".to_owned(),
                "consistency".to_owned(),
                "conditional exchangeability or randomized assignment".to_owned(),
                "positivity".to_owned(),
                "no unmodeled interference for the declared estimand".to_owned(),
            ],
            streams,
            expected_pair_count,
            pairs,
            pc_stable_skeleton,
            partial_correlation_network,
            hawkes_branching_graph,
            fdr_families,
        };
        if !artifact.all_stream_pairs_enumerated {
            return Err(causal_error(
                "SYNAPSE_CALYX_CAUSAL_MAP_PAIR_COVERAGE_MISMATCH",
                format!(
                    "pair enumeration produced {} rows but C(n,2) requires {}",
                    artifact.pairs.len(),
                    artifact.expected_pair_count
                ),
                "inspect the deterministic stream-pair enumeration before publishing any causal artifact",
            ));
        }
        let bytes = serde_json::to_vec(&artifact).map_err(|error| {
            causal_error(
                "SYNAPSE_CALYX_CAUSAL_MAP_ENCODE_FAILED",
                format!("encode causal-map artifact: {error}"),
                "inspect the causal-map report for a non-serializable or non-finite field",
            )
        })?;
        let digest = Sha256::digest(&bytes);
        let graph_key = causal_map_key(params.panel_version, &digest);
        let graph_key_hex = hex_encode(&graph_key);
        let graph_value_sha256 = hex_encode(&digest);
        let pointer_key = causal_map_pointer_key(&scope)?;
        let pointer = SynapseCalyxCausalMapPointer {
            schema: CAUSAL_MAP_POINTER_SCHEMA.to_owned(),
            scope_sha256: scope.scope_sha256.clone(),
            panel_version: scope.panel_version,
            group_key: scope.group_key.clone(),
            pair_scope: scope.pair_scope.clone(),
            group_a: scope.group_a.clone(),
            group_b: scope.group_b.clone(),
            window_kind: scope.window_kind.clone(),
            window_span_ns: scope.window_span_ns,
            bin_seconds_bits: scope.bin_seconds.to_bits(),
            max_lag: scope.max_lag,
            fdr_alpha_bits: scope.fdr_alpha.to_bits(),
            artifact_key_hex: graph_key_hex.clone(),
            artifact_sha256: graph_value_sha256.clone(),
            source_fingerprint_sha256: artifact.source_fingerprint_sha256.clone(),
            source_records: artifact.source_records,
            earliest_event_ns: artifact.earliest_event_ns,
            latest_event_ns: artifact.latest_event_ns,
            since_ts_ns: artifact.since_ts_ns,
            until_ts_ns: artifact.until_ts_ns,
        };
        let existing_pointer = self.read_cf_latest_revisioned(ColumnFamily::Graph, &pointer_key)?;
        if let Some(existing_row) = &existing_pointer {
            let existing = decode_causal_map_pointer(&existing_row.value)?;
            validate_pointer_scope(&existing, &scope)?;
            validate_pointer_advance(&existing, &pointer)?;
        }
        let pointer_bytes = serde_json::to_vec(&pointer).map_err(|error| {
            causal_error(
                "SYNAPSE_CALYX_CAUSAL_MAP_POINTER_ENCODE_FAILED",
                format!("encode causal-map scope pointer: {error}"),
                "inspect the normalized causal-map scope and artifact identity before publishing",
            )
        })?;
        let pointer_digest = Sha256::digest(&pointer_bytes);
        // One revision-guarded native batch is the publication boundary:
        // readers can observe neither row or both rows, never a pointer whose
        // immutable artifact is absent, and a concurrent publisher cannot
        // replace a newer frontier with the candidate validated above. The
        // artifact is listed first to preserve the manifest model even for
        // diagnostic write-order inspection.
        let expected_pointer_revision = existing_pointer.map(|row| row.revision_sha256);
        let publication = self.write_cf_batch_if_revision(
            ColumnFamily::Graph,
            &pointer_key,
            expected_pointer_revision,
            vec![
                SynapseCalyxCfWrite {
                    cf: ColumnFamily::Graph,
                    key: graph_key.clone(),
                    value: bytes.clone(),
                },
                SynapseCalyxCfWrite {
                    cf: ColumnFamily::Graph,
                    key: pointer_key.clone(),
                    value: pointer_bytes.clone(),
                },
            ],
        )?;
        if !publication.applied {
            return Err(causal_error(
                "SYNAPSE_CALYX_CAUSAL_MAP_POINTER_REVISION_CONFLICT",
                format!(
                    "normalized-scope pointer {} changed after its frontier was validated; expected revision {}, actual revision {}",
                    hex_encode(&pointer_key),
                    expected_pointer_revision
                        .as_ref()
                        .map_or_else(|| "absent".to_owned(), |value| hex_encode(value)),
                    publication
                        .previous_revision_sha256
                        .as_ref()
                        .map_or_else(|| "absent".to_owned(), |value| hex_encode(value)),
                ),
                "read the current normalized-scope pointer and recompute from an equal-or-newer source-event frontier; never overwrite a concurrent publication",
            ));
        }
        self.flush()?;
        let physical = self.read_cf_latest(ColumnFamily::Graph, &graph_key)?;
        if physical.as_deref() != Some(bytes.as_slice()) {
            tracing::error!(
                code = "SYNAPSE_CALYX_CAUSAL_MAP_READBACK_MISMATCH",
                panel_version = params.panel_version,
                graph_key_hex = hex_encode(&graph_key),
                "persisted causal-map bytes differ from the separate Graph-CF read"
            );
            return Err(causal_error(
                "SYNAPSE_CALYX_CAUSAL_MAP_READBACK_MISMATCH",
                "the Graph-CF row read after persistence does not byte-match the causal-map artifact",
                "inspect the exact Graph CF key, vault manifest/WAL, and concurrent writers before retrying",
            ));
        }
        let physical_pointer = self.read_cf_latest(ColumnFamily::Graph, &pointer_key)?;
        if physical_pointer.as_deref() != Some(pointer_bytes.as_slice()) {
            tracing::error!(
                code = "SYNAPSE_CALYX_CAUSAL_MAP_POINTER_READBACK_MISMATCH",
                panel_version = params.panel_version,
                pointer_key_hex = hex_encode(&pointer_key),
                "persisted causal-map pointer bytes differ from the separate Graph-CF read"
            );
            return Err(causal_error(
                "SYNAPSE_CALYX_CAUSAL_MAP_POINTER_READBACK_MISMATCH",
                "the Graph-CF row read after publication does not byte-match the causal-map scope pointer",
                "inspect the exact Graph CF pointer key, vault manifest/WAL, and concurrent writers before retrying",
            ));
        }
        // A byte-exact publication can still already be stale if a late source
        // event entered the closed window while the estimators ran. Re-read and
        // fingerprint that physical source population before returning; the
        // durable pointer remains fail-closed because every subsequent reader
        // performs this same validation.
        validate_artifact_source(self, &artifact, &pointer)?;
        let graph_cf_rows_after = self
            .count_cf_latest_bounded_memoized(ColumnFamily::Graph)?
            .rows();
        let pointer_key_hex = hex_encode(&pointer_key);
        let pointer_value_sha256 = hex_encode(&pointer_digest);
        tracing::info!(
            code = "SYNAPSE_CALYX_CAUSAL_MAP_PERSISTED_AND_READ_BACK",
            panel_version = params.panel_version,
            source_records = artifact.source_records,
            stream_count = artifact.streams.len(),
            pair_count = artifact.pairs.len(),
            graph_key_hex,
            graph_value_sha256,
            graph_value_bytes = bytes.len(),
            pointer_key_hex,
            pointer_value_sha256,
            pointer_value_bytes = pointer_bytes.len(),
            graph_cf_rows_after,
            "completed exhaustive causal map and byte-exact artifact-plus-pointer readback"
        );
        Ok(SynapseCalyxCausalMapReport {
            artifact,
            graph_key_hex,
            graph_value_sha256,
            graph_value_bytes: bytes.len(),
            pointer_key_hex,
            pointer_value_sha256,
            pointer_value_bytes: pointer_bytes.len(),
            graph_cf_rows_after,
            physical_readback_matches: true,
            pointer_readback_matches: true,
        })
    }

    /// Resolves the latest published generation for one normalized causal-map
    /// contract and validates its immutable bytes plus its exact source-event
    /// population. No estimator is rerun and no state is written.
    ///
    /// # Errors
    ///
    /// Fails closed when the scope has never been published, either pointer or
    /// artifact is missing/corrupt/mismatched, the artifact is incomplete, or
    /// the source population inside its closed event window has drifted.
    #[allow(clippy::too_many_lines)]
    pub fn read_temporal_causal_map(
        &self,
        params: &SynapseCalyxTemporalParams,
        fdr_alpha: f32,
    ) -> Result<SynapseCalyxCausalMapReport, SynapseCalyxError> {
        crate::lowering::hot_context::assert_cold_calyx("read_temporal_causal_map");
        let scope = causal_map_scope(params, fdr_alpha)?;
        let pointer_key = causal_map_pointer_key(&scope)?;
        let pointer_key_hex = hex_encode(&pointer_key);
        let pointer_bytes = self
            .read_cf_latest(ColumnFamily::Graph, &pointer_key)?
            .ok_or_else(|| {
                causal_error(
                    "SYNAPSE_CALYX_CAUSAL_MAP_NOT_BUILT",
                    format!(
                        "no causal-map pointer exists for normalized scope {} at Graph key {pointer_key_hex}",
                        scope.scope_sha256
                    ),
                    "run storage intelligence causal_map for this exact panel/group/pair/window-shape/bin/lag/FDR contract before reading it",
                )
            })?;
        let pointer = decode_causal_map_pointer(&pointer_bytes)?;
        validate_pointer_scope(&pointer, &scope)?;
        validate_requested_source_limit(pointer.source_records, params.max_records)?;
        let artifact_digest = decode_sha256(&pointer.artifact_sha256)?;
        let artifact_key = causal_map_key(pointer.panel_version, &artifact_digest);
        if hex_encode(&artifact_key) != pointer.artifact_key_hex {
            return Err(causal_error(
                "SYNAPSE_CALYX_CAUSAL_MAP_POINTER_ARTIFACT_KEY_MISMATCH",
                format!(
                    "pointer artifact key {} is not derived from panel {} and artifact SHA-256 {}",
                    pointer.artifact_key_hex, pointer.panel_version, pointer.artifact_sha256
                ),
                "rebuild the exact causal-map scope; never follow a pointer whose key is not content-addressed by its declared artifact",
            ));
        }
        let artifact_bytes = self
            .read_cf_latest(ColumnFamily::Graph, &artifact_key)?
            .ok_or_else(|| {
                causal_error(
                    "SYNAPSE_CALYX_CAUSAL_MAP_ARTIFACT_MISSING",
                    format!(
                        "causal-map pointer {pointer_key_hex} references absent Graph artifact {}",
                        pointer.artifact_key_hex
                    ),
                    "restore or rebuild the exact immutable artifact; reads never recompute a missing generation",
                )
            })?;
        let actual_digest = Sha256::digest(&artifact_bytes);
        if actual_digest.as_slice() != artifact_digest {
            return Err(causal_error(
                "SYNAPSE_CALYX_CAUSAL_MAP_ARTIFACT_HASH_MISMATCH",
                format!(
                    "Graph artifact {} hashes to {} but its pointer declares {}",
                    pointer.artifact_key_hex,
                    hex_encode(&actual_digest),
                    pointer.artifact_sha256
                ),
                "preserve the corrupt rows for diagnosis, then rebuild the exact causal-map scope from authoritative Base records",
            ));
        }
        let artifact: SynapseCalyxCausalMapArtifact = serde_json::from_slice(&artifact_bytes)
            .map_err(|error| {
                causal_error(
                    "SYNAPSE_CALYX_CAUSAL_MAP_ARTIFACT_DECODE_FAILED",
                    format!("decode pointed causal-map artifact: {error}"),
                    "preserve the corrupt Graph row and rebuild the exact causal-map scope",
                )
            })?;
        validate_artifact_contract(&artifact, &pointer, &scope)?;
        validate_artifact_source(self, &artifact, &pointer)?;

        let pointer_digest = Sha256::digest(&pointer_bytes);
        let graph_cf_rows_after = self
            .count_cf_latest_bounded_memoized(ColumnFamily::Graph)?
            .rows();
        tracing::info!(
            code = "SYNAPSE_CALYX_CAUSAL_MAP_GENERATION_READ",
            panel_version = artifact.panel_version,
            scope_sha256 = pointer.scope_sha256,
            pointer_key_hex,
            artifact_key_hex = pointer.artifact_key_hex,
            artifact_sha256 = pointer.artifact_sha256,
            source_fingerprint_sha256 = artifact.source_fingerprint_sha256,
            source_records = artifact.source_records,
            latest_event_ns = artifact.latest_event_ns,
            "read and independently validated one persisted causal-map generation"
        );
        Ok(SynapseCalyxCausalMapReport {
            graph_key_hex: pointer.artifact_key_hex,
            graph_value_sha256: pointer.artifact_sha256,
            graph_value_bytes: artifact_bytes.len(),
            pointer_key_hex,
            pointer_value_sha256: hex_encode(&pointer_digest),
            pointer_value_bytes: pointer_bytes.len(),
            graph_cf_rows_after,
            physical_readback_matches: true,
            pointer_readback_matches: true,
            artifact,
        })
    }
}

fn causal_map_scope(
    params: &SynapseCalyxTemporalParams,
    fdr_alpha: f32,
) -> Result<CausalMapScope, SynapseCalyxError> {
    validate_fdr_alpha(fdr_alpha)?;
    if !(1..=crate::SYNAPSE_INTELLIGENCE_MAX_RECORDS).contains(&params.max_records) {
        return Err(causal_error(
            "SYNAPSE_CALYX_CAUSAL_MAP_RECORD_LIMIT_INVALID",
            format!(
                "max_records={} is outside 1..={}",
                params.max_records,
                crate::SYNAPSE_INTELLIGENCE_MAX_RECORDS
            ),
            "supply a bounded positive max_records value; causal-map measurements never clamp a caller's contract",
        ));
    }
    let group_key = required_trimmed_group_key(params)?.to_owned();
    let normalized_pair =
        requested_pair_scope(params)?.map(|(a, b)| if a <= b { (a, b) } else { (b, a) });
    let (pair_scope, group_a, group_b) = normalized_pair.map_or_else(
        || ("complete".to_owned(), None, None),
        |(a, b)| ("explicit_pair".to_owned(), Some(a), Some(b)),
    );
    let bin_seconds = validate_bin_seconds(params.bin_seconds)?;
    let max_lag = validate_max_lag(params.max_lag)?;
    let (window_kind, window_span_ns) = match (params.since_ts_ns, params.until_ts_ns) {
        (Some(since), Some(until)) if since < until => (
            "bounded_span".to_owned(),
            Some(until.checked_sub(since).ok_or_else(|| {
                causal_error(
                    "SYNAPSE_CALYX_CAUSAL_MAP_WINDOW_SPAN_OVERFLOW",
                    "bounded causal-map window span overflowed i64 nanoseconds",
                    "supply representable Unix-nanosecond bounds with since_ts_ns < until_ts_ns",
                )
            })?),
        ),
        (Some(_), Some(_)) => {
            return Err(causal_error(
                "SYNAPSE_CALYX_CAUSAL_MAP_TIME_RANGE_INVALID",
                "since_ts_ns must be strictly less than until_ts_ns",
                "supply an inclusive lower bound below the exclusive upper bound",
            ));
        }
        (Some(_), None) => ("since_only".to_owned(), None),
        (None, Some(_)) => ("until_only".to_owned(), None),
        (None, None) => ("unbounded".to_owned(), None),
    };

    let mut encoded = Vec::new();
    append_scope_part(&mut encoded, &params.panel_version.to_be_bytes());
    append_scope_part(&mut encoded, group_key.as_bytes());
    append_scope_part(&mut encoded, pair_scope.as_bytes());
    append_optional_scope_part(&mut encoded, group_a.as_deref());
    append_optional_scope_part(&mut encoded, group_b.as_deref());
    append_scope_part(&mut encoded, window_kind.as_bytes());
    match window_span_ns {
        Some(span) => {
            encoded.push(1);
            append_scope_part(&mut encoded, &span.to_be_bytes());
        }
        None => encoded.push(0),
    }
    append_scope_part(&mut encoded, &bin_seconds.to_bits().to_be_bytes());
    let max_lag_u64 = u64::try_from(max_lag).map_err(|_| {
        causal_error(
            "SYNAPSE_CALYX_CAUSAL_MAP_SCOPE_ENCODE_FAILED",
            "max_lag cannot be represented as u64",
            "repair the platform integer conversion before publishing causal-map state",
        )
    })?;
    append_scope_part(&mut encoded, &max_lag_u64.to_be_bytes());
    append_scope_part(&mut encoded, &fdr_alpha.to_bits().to_be_bytes());
    let scope_sha256 = hex_encode(&Sha256::digest(&encoded));
    Ok(CausalMapScope {
        scope_sha256,
        panel_version: params.panel_version,
        group_key,
        pair_scope,
        group_a,
        group_b,
        window_kind,
        window_span_ns,
        bin_seconds,
        max_lag,
        fdr_alpha,
    })
}

fn append_scope_part(encoded: &mut Vec<u8>, value: &[u8]) {
    encoded.extend_from_slice(&(value.len() as u64).to_be_bytes());
    encoded.extend_from_slice(value);
}

fn append_optional_scope_part(encoded: &mut Vec<u8>, value: Option<&str>) {
    match value {
        Some(value) => {
            encoded.push(1);
            append_scope_part(encoded, value.as_bytes());
        }
        None => encoded.push(0),
    }
}

fn validate_max_lag(max_lag: usize) -> Result<usize, SynapseCalyxError> {
    if (1..=SYNAPSE_TEMPORAL_MAX_LAGS).contains(&max_lag) {
        Ok(max_lag)
    } else {
        Err(causal_error(
            "SYNAPSE_CALYX_CAUSAL_MAP_MAX_LAG_INVALID",
            format!("max_lag={max_lag} is outside 1..={SYNAPSE_TEMPORAL_MAX_LAGS}"),
            "supply a supported positive lag count; causal-map measurements never clamp a caller's contract",
        ))
    }
}

fn causal_map_pointer_key(scope: &CausalMapScope) -> Result<Vec<u8>, SynapseCalyxError> {
    let digest = decode_sha256(&scope.scope_sha256)?;
    let mut key = Vec::with_capacity(GRAPH_CAUSAL_MAP_INDEX_PREFIX.len() + 4 + 16);
    key.extend_from_slice(GRAPH_CAUSAL_MAP_INDEX_PREFIX);
    key.extend_from_slice(&scope.panel_version.to_be_bytes());
    key.extend_from_slice(&digest[..16]);
    Ok(key)
}

fn decode_causal_map_pointer(
    bytes: &[u8],
) -> Result<SynapseCalyxCausalMapPointer, SynapseCalyxError> {
    let pointer: SynapseCalyxCausalMapPointer = serde_json::from_slice(bytes).map_err(|error| {
        causal_error(
            "SYNAPSE_CALYX_CAUSAL_MAP_POINTER_DECODE_FAILED",
            format!("decode causal-map pointer row: {error}"),
            "preserve the corrupt Graph row and rebuild the exact causal-map scope",
        )
    })?;
    if pointer.schema != CAUSAL_MAP_POINTER_SCHEMA {
        return Err(causal_error(
            "SYNAPSE_CALYX_CAUSAL_MAP_POINTER_SCHEMA_UNSUPPORTED",
            format!(
                "causal-map pointer schema {:?} is unsupported; expected {CAUSAL_MAP_POINTER_SCHEMA}",
                pointer.schema
            ),
            "rebuild the exact causal-map scope with the current runtime; reads never infer a legacy pointer contract",
        ));
    }
    Ok(pointer)
}

fn validate_pointer_scope(
    pointer: &SynapseCalyxCausalMapPointer,
    scope: &CausalMapScope,
) -> Result<(), SynapseCalyxError> {
    if pointer.scope_sha256 != scope.scope_sha256
        || pointer.panel_version != scope.panel_version
        || pointer.group_key != scope.group_key
        || pointer.pair_scope != scope.pair_scope
        || pointer.group_a != scope.group_a
        || pointer.group_b != scope.group_b
        || pointer.window_kind != scope.window_kind
        || pointer.window_span_ns != scope.window_span_ns
        || pointer.bin_seconds_bits != scope.bin_seconds.to_bits()
        || pointer.max_lag != scope.max_lag
        || pointer.fdr_alpha_bits != scope.fdr_alpha.to_bits()
    {
        return Err(causal_error(
            "SYNAPSE_CALYX_CAUSAL_MAP_POINTER_SCOPE_MISMATCH",
            format!(
                "causal-map pointer scope {} does not match requested normalized scope {}",
                pointer.scope_sha256, scope.scope_sha256
            ),
            "rebuild the exact requested causal-map contract; never serve an artifact through a cross-scope pointer",
        ));
    }
    Ok(())
}

fn validate_requested_source_limit(
    source_records: usize,
    requested_max_records: usize,
) -> Result<(), SynapseCalyxError> {
    if source_records > requested_max_records {
        return Err(causal_error(
            "SYNAPSE_CALYX_CAUSAL_MAP_RECORD_LIMIT_EXCEEDED",
            format!(
                "the persisted causal-map population contains {source_records} source records, exceeding the caller's max_records={requested_max_records} bound"
            ),
            "narrow the source-event-time window or explicitly raise max_records to the reported source population without exceeding the system ceiling",
        ));
    }
    Ok(())
}

fn validate_pointer_advance(
    existing: &SynapseCalyxCausalMapPointer,
    candidate: &SynapseCalyxCausalMapPointer,
) -> Result<(), SynapseCalyxError> {
    if candidate.latest_event_ns < existing.latest_event_ns {
        return Err(causal_error(
            "SYNAPSE_CALYX_CAUSAL_MAP_POINTER_REGRESSION",
            format!(
                "candidate latest_event_ns={} regresses current frontier {} for scope {}",
                candidate.latest_event_ns, existing.latest_event_ns, existing.scope_sha256
            ),
            "publish a source-event window whose latest physical event does not precede the current materialized generation",
        ));
    }
    let existing_boundary = pointer_publication_boundary(existing);
    let candidate_boundary = pointer_publication_boundary(candidate);
    if candidate_boundary < existing_boundary {
        return Err(causal_error(
            "SYNAPSE_CALYX_CAUSAL_MAP_POINTER_REGRESSION",
            format!(
                "candidate publication boundary {candidate_boundary} regresses current boundary {existing_boundary} for scope {}",
                existing.scope_sha256
            ),
            "publish a window at or after the current normalized-scope boundary; historical windows remain addressable by immutable artifact key",
        ));
    }
    if candidate_boundary == existing_boundary
        && candidate.latest_event_ns == existing.latest_event_ns
        && candidate.artifact_sha256 != existing.artifact_sha256
    {
        return Err(causal_error(
            "SYNAPSE_CALYX_CAUSAL_MAP_POINTER_FRONTIER_AMBIGUOUS",
            format!(
                "scope {} produced two different artifacts at the same publication/event frontier: current={} candidate={}",
                existing.scope_sha256, existing.artifact_sha256, candidate.artifact_sha256
            ),
            "inspect the exact source fingerprint and analysis contract; equal frontiers must reproduce byte-identical artifacts",
        ));
    }
    Ok(())
}

fn pointer_publication_boundary(pointer: &SynapseCalyxCausalMapPointer) -> i128 {
    match pointer.window_kind.as_str() {
        "bounded_span" | "until_only" => pointer.until_ts_ns.map_or(i128::MIN, i128::from),
        "since_only" => pointer.since_ts_ns.map_or(i128::MIN, i128::from),
        _ => i128::from(pointer.latest_event_ns),
    }
}

fn source_fingerprint(
    records: &[crate::intelligence::EventRecord],
) -> Result<String, SynapseCalyxError> {
    let mut members = records
        .iter()
        .map(|record| {
            record
                .group
                .as_deref()
                .map(|group| (record.nanos, group.to_owned()))
                .ok_or_else(|| {
                    causal_error(
                        "SYNAPSE_CALYX_CAUSAL_MAP_GROUP_VALUE_MISSING",
                        "a source-event member has no group value while fingerprinting the complete causal population",
                        "choose a group_key present on every in-scope record; incomplete populations are never fingerprinted",
                    )
                })
        })
        .collect::<Result<Vec<_>, _>>()?;
    members.sort();
    let mut hasher = Sha256::new();
    for (nanos, group) in members {
        append_digest_part(&mut hasher, &nanos.to_be_bytes());
        append_digest_part(&mut hasher, group.as_bytes());
    }
    Ok(hex_encode(&hasher.finalize()))
}

fn append_digest_part(hasher: &mut Sha256, value: &[u8]) {
    hasher.update((value.len() as u64).to_be_bytes());
    hasher.update(value);
}

fn decode_sha256(value: &str) -> Result<[u8; 32], SynapseCalyxError> {
    if value.len() != 64 {
        return Err(causal_error(
            "SYNAPSE_CALYX_CAUSAL_MAP_SHA256_INVALID",
            format!(
                "expected 64 lowercase hexadecimal characters, got length {}",
                value.len()
            ),
            "rebuild the exact causal-map scope; never follow an invalid content identity",
        ));
    }
    let bytes = value.as_bytes();
    let mut decoded = [0_u8; 32];
    for (index, slot) in decoded.iter_mut().enumerate() {
        let high = decode_hex_nibble(bytes[index * 2])?;
        let low = decode_hex_nibble(bytes[index * 2 + 1])?;
        *slot = (high << 4) | low;
    }
    Ok(decoded)
}

fn decode_hex_nibble(value: u8) -> Result<u8, SynapseCalyxError> {
    match value {
        b'0'..=b'9' => Ok(value - b'0'),
        b'a'..=b'f' => Ok(value - b'a' + 10),
        _ => Err(causal_error(
            "SYNAPSE_CALYX_CAUSAL_MAP_SHA256_INVALID",
            "content identity contains a non-lowercase-hexadecimal character",
            "rebuild the exact causal-map scope; never normalize a malformed persisted identity",
        )),
    }
}

fn validate_artifact_contract(
    artifact: &SynapseCalyxCausalMapArtifact,
    pointer: &SynapseCalyxCausalMapPointer,
    scope: &CausalMapScope,
) -> Result<(), SynapseCalyxError> {
    if artifact.schema != CAUSAL_MAP_ARTIFACT_SCHEMA
        || artifact.panel_version != scope.panel_version
        || artifact.group_key != scope.group_key
        || artifact.pair_scope != scope.pair_scope
        || artifact.since_ts_ns != pointer.since_ts_ns
        || artifact.until_ts_ns != pointer.until_ts_ns
        || artifact.bin_seconds.to_bits() != scope.bin_seconds.to_bits()
        || artifact.max_lag != scope.max_lag
        || artifact.fdr_alpha.to_bits() != scope.fdr_alpha.to_bits()
        || artifact.source_records != pointer.source_records
        || artifact.source_fingerprint_sha256 != pointer.source_fingerprint_sha256
        || artifact.earliest_event_ns != pointer.earliest_event_ns
        || artifact.latest_event_ns != pointer.latest_event_ns
    {
        return Err(artifact_contract_error(
            "the pointed causal-map artifact does not byte-semantically match its normalized scope pointer",
        ));
    }
    if !(1..=crate::SYNAPSE_INTELLIGENCE_MAX_RECORDS).contains(&artifact.max_records)
        || artifact.source_records > artifact.max_records
    {
        return Err(artifact_contract_error(
            "the pointed causal-map artifact carries an invalid max-records contract or more source records than that contract permits",
        ));
    }
    decode_sha256(&artifact.source_fingerprint_sha256)?;
    if !artifact.all_requested_records_loaded
        || !artifact.all_stream_pairs_enumerated
        || artifact.evidence_class != "observational_predictive"
        || artifact.structural_effect_identified
        || artifact.identification_requirements.is_empty()
    {
        return Err(artifact_contract_error(
            "the pointed causal-map artifact does not retain complete observational-only coverage and identification boundaries",
        ));
    }
    validate_stream_and_pair_coverage(artifact, pointer)?;
    validate_artifact_evidence(artifact)?;
    validate_fdr_families(artifact)?;
    Ok(())
}

fn validate_stream_and_pair_coverage(
    artifact: &SynapseCalyxCausalMapArtifact,
    pointer: &SynapseCalyxCausalMapPointer,
) -> Result<(), SynapseCalyxError> {
    let stream_names = artifact
        .streams
        .iter()
        .map(|stream| stream.name.clone())
        .collect::<BTreeSet<_>>();
    if stream_names.len() != artifact.streams.len()
        || artifact
            .streams
            .iter()
            .any(|stream| stream.event_count == 0)
    {
        return Err(artifact_contract_error(
            "causal-map stream identities are duplicated or carry zero physical events",
        ));
    }
    if pointer.pair_scope == "explicit_pair" {
        let expected = pointer
            .group_a
            .iter()
            .chain(pointer.group_b.iter())
            .cloned()
            .collect::<BTreeSet<_>>();
        if stream_names != expected || expected.len() != 2 {
            return Err(artifact_contract_error(
                "explicit-pair pointer endpoints do not equal the artifact's two stream identities",
            ));
        }
    } else if pointer.group_a.is_some() || pointer.group_b.is_some() {
        return Err(artifact_contract_error(
            "complete causal-map pointer unexpectedly carries explicit pair endpoints",
        ));
    }

    let expected_pair_count = artifact.streams.len() * artifact.streams.len().saturating_sub(1) / 2;
    if artifact.expected_pair_count != expected_pair_count
        || artifact.pairs.len() != expected_pair_count
    {
        return Err(artifact_contract_error(
            "causal-map pair cardinality does not equal C(stream_count,2)",
        ));
    }
    let mut observed_pairs = BTreeSet::new();
    for pair in &artifact.pairs {
        if pair.group_a >= pair.group_b
            || !stream_names.contains(&pair.group_a)
            || !stream_names.contains(&pair.group_b)
            || !observed_pairs.insert((pair.group_a.clone(), pair.group_b.clone()))
        {
            return Err(artifact_contract_error(
                "causal-map pair identities are non-canonical, outside the stream roster, or duplicated",
            ));
        }
        let events_a = artifact
            .streams
            .iter()
            .find(|stream| stream.name == pair.group_a)
            .map(|stream| stream.event_count);
        let events_b = artifact
            .streams
            .iter()
            .find(|stream| stream.name == pair.group_b)
            .map(|stream| stream.event_count);
        if events_a != Some(pair.events_a) || events_b != Some(pair.events_b) {
            return Err(artifact_contract_error(
                "causal-map pair event counts disagree with the stream roster",
            ));
        }
    }
    Ok(())
}

fn validate_artifact_evidence(
    artifact: &SynapseCalyxCausalMapArtifact,
) -> Result<(), SynapseCalyxError> {
    for pair in &artifact.pairs {
        for evidence in [
            &pair.transfer_entropy,
            &pair.granger_a_to_b,
            &pair.granger_b_to_a,
            &pair.cross_correlation,
            &pair.convergent_cross_mapping,
            &pair.temporal_cross_k,
        ] {
            validate_evidence_state(evidence)?;
        }
    }
    for evidence in [
        &artifact.pc_stable_skeleton,
        &artifact.partial_correlation_network,
        &artifact.hawkes_branching_graph,
    ] {
        validate_evidence_state(evidence)?;
    }
    Ok(())
}

fn validate_evidence_state(
    evidence: &SynapseCalyxCausalEstimatorEvidence,
) -> Result<(), SynapseCalyxError> {
    let valid = match evidence.status.as_str() {
        "measured" => evidence.result.is_some() && evidence.error.is_none(),
        "failed" => evidence.result.is_none() && evidence.error.is_some(),
        "unresolved" => evidence.result.is_some() && evidence.error.is_some(),
        _ => false,
    };
    if evidence.estimator.trim().is_empty() || evidence.assumptions.is_empty() || !valid {
        return Err(artifact_contract_error(
            "a causal estimator lane has an unknown/internally inconsistent status or omits its estimator/assumptions",
        ));
    }
    if let Some(error) = &evidence.error
        && (error.code.trim().is_empty()
            || error.message.trim().is_empty()
            || error.remediation.trim().is_empty())
    {
        return Err(artifact_contract_error(
            "a failed or unresolved causal estimator lane omits its typed error/remediation",
        ));
    }
    Ok(())
}

fn validate_fdr_families(
    artifact: &SynapseCalyxCausalMapArtifact,
) -> Result<(), SynapseCalyxError> {
    let required = BTreeSet::from([
        "granger_all_pairs_directions_lags",
        "cross_correlation_all_pairs_lags",
        "pc_stable_removed_edge_tests",
        "partial_correlation_all_pairs",
    ]);
    let observed = artifact
        .fdr_families
        .iter()
        .map(|family| family.name.as_str())
        .collect::<BTreeSet<_>>();
    if observed != required || artifact.fdr_families.len() != required.len() {
        return Err(artifact_contract_error(
            "the causal-map artifact does not contain exactly the four declared BH-FDR hypothesis families",
        ));
    }
    for family in &artifact.fdr_families {
        if family.method != "benjamini_hochberg"
            || family.assumptions
                != [
                    "FDR control assumes independent or positive-regression-dependent p-values within this named family",
                    "multiplicity is controlled within this family only; no cross-family error-rate claim is made",
                ]
            || family.alpha.to_bits() != artifact.fdr_alpha.to_bits()
            || family.hypotheses_tested != family.decisions.len()
        {
            return Err(artifact_contract_error(
                "a causal-map BH-FDR family does not match the artifact alpha/method/cardinality contract",
            ));
        }
        let mut hypotheses = BTreeSet::new();
        for decision in &family.decisions {
            if decision.hypothesis.trim().is_empty()
                || !hypotheses.insert(decision.hypothesis.as_str())
                || !decision.p_value.is_finite()
                || !(0.0..=1.0).contains(&decision.p_value)
                || !decision.q_value.is_finite()
                || !(0.0..=1.0).contains(&decision.q_value)
                || decision.significant != (decision.q_value <= family.alpha)
            {
                return Err(artifact_contract_error(
                    "a causal-map BH-FDR decision is duplicated, non-finite, outside [0,1], or inconsistent with alpha",
                ));
            }
        }
    }
    Ok(())
}

fn validate_artifact_source(
    vault: &SynapseCalyxVault,
    artifact: &SynapseCalyxCausalMapArtifact,
    pointer: &SynapseCalyxCausalMapPointer,
) -> Result<(), SynapseCalyxError> {
    let mut params = SynapseCalyxTemporalParams::new(artifact.panel_version);
    params.max_records = artifact.max_records;
    params.since_ts_ns = artifact.since_ts_ns;
    params.until_ts_ns = artifact.until_ts_ns;
    params.group_key = Some(artifact.group_key.clone());
    params.group_a.clone_from(&pointer.group_a);
    params.group_b.clone_from(&pointer.group_b);
    params.bin_seconds = artifact.bin_seconds;
    params.max_lag = artifact.max_lag;
    let records = vault.load_panel_event_records(&params, Some(&artifact.group_key))?;
    if records.is_empty() {
        return Err(source_stale_error(
            artifact,
            "the persisted artifact's exact source-event window is now empty",
        ));
    }
    let fingerprint = source_fingerprint(&records)?;
    let earliest = records.iter().map(|record| record.nanos).min();
    let latest = records.iter().map(|record| record.nanos).max();
    if records.len() != artifact.source_records
        || fingerprint != artifact.source_fingerprint_sha256
        || earliest != Some(artifact.earliest_event_ns)
        || latest != Some(artifact.latest_event_ns)
    {
        return Err(source_stale_error(
            artifact,
            &format!(
                "persisted source records/fingerprint/frontier = {}/{}/{:?}..{:?}, current = {}/{}/{earliest:?}..{latest:?}",
                artifact.source_records,
                artifact.source_fingerprint_sha256,
                Some(artifact.earliest_event_ns),
                Some(artifact.latest_event_ns),
                records.len(),
                fingerprint,
            ),
        ));
    }
    let mut counts = BTreeMap::<String, usize>::new();
    for record in &records {
        let Some(group) = record.group.as_ref() else {
            return Err(source_stale_error(
                artifact,
                "a current in-window source event has no required group value",
            ));
        };
        *counts.entry(group.clone()).or_default() += 1;
    }
    let series = counts
        .keys()
        .map(|group| (group.clone(), Vec::new()))
        .collect::<BTreeMap<_, _>>();
    let pair_scope = pointer.group_a.clone().zip(pointer.group_b.clone());
    let selected = select_streams(&series, pair_scope.as_ref())?;
    let expected_streams = selected
        .into_iter()
        .map(|name| SynapseCalyxCausalStream {
            event_count: counts[&name],
            name,
        })
        .collect::<Vec<_>>();
    if expected_streams != artifact.streams {
        return Err(source_stale_error(
            artifact,
            "current source stream identities/counts differ from the persisted artifact",
        ));
    }
    Ok(())
}

fn artifact_contract_error(message: impl Into<String>) -> SynapseCalyxError {
    causal_error(
        "SYNAPSE_CALYX_CAUSAL_MAP_ARTIFACT_CONTRACT_INVALID",
        message,
        "preserve the invalid Graph rows for diagnosis, then rebuild the exact causal-map scope; reads never infer missing evidence",
    )
}

fn source_stale_error(artifact: &SynapseCalyxCausalMapArtifact, detail: &str) -> SynapseCalyxError {
    causal_error(
        "SYNAPSE_CALYX_CAUSAL_MAP_SOURCE_STALE",
        format!(
            "causal-map source population drifted for panel {} group_key={}: {detail}",
            artifact.panel_version, artifact.group_key
        ),
        "recompute and atomically publish the normalized causal-map scope from the current authoritative Base population",
    )
}

fn required_trimmed_group_key(
    params: &SynapseCalyxTemporalParams,
) -> Result<&str, SynapseCalyxError> {
    params
        .group_key
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            causal_error(
                "SYNAPSE_CALYX_CAUSAL_MAP_GROUP_KEY_REQUIRED",
                "causal_map requires a non-empty metadata group_key defining independently observed streams",
                "supply a metadata key present on every record in the requested source-event scope",
            )
        })
}

fn requested_pair_scope(
    params: &SynapseCalyxTemporalParams,
) -> Result<Option<(String, String)>, SynapseCalyxError> {
    match (&params.group_a, &params.group_b) {
        (None, None) => Ok(None),
        (Some(a), Some(b))
            if !a.trim().is_empty() && !b.trim().is_empty() && a.trim() != b.trim() =>
        {
            Ok(Some((a.trim().to_owned(), b.trim().to_owned())))
        }
        (Some(a), Some(b)) if !a.trim().is_empty() && a.trim() == b.trim() => Err(causal_error(
            "SYNAPSE_CALYX_CAUSAL_MAP_STREAMS_IDENTICAL",
            "group_a and group_b identify the same stream",
            "supply two distinct stream values or omit both to enumerate the complete map",
        )),
        _ => Err(causal_error(
            "SYNAPSE_CALYX_CAUSAL_MAP_PAIR_SCOPE_INCOMPLETE",
            "an explicit causal pair requires both non-empty group_a and group_b; one-sided selection would silently change the requested scope",
            "supply both group_a and group_b, or omit both for exhaustive C(n,2) enumeration",
        )),
    }
}

fn select_streams(
    by_group: &BTreeMap<String, Vec<f64>>,
    pair_scope: Option<&(String, String)>,
) -> Result<Vec<String>, SynapseCalyxError> {
    if let Some((a, b)) = pair_scope {
        for requested in [a, b] {
            if !by_group.contains_key(requested) {
                return Err(causal_error(
                    "SYNAPSE_CALYX_CAUSAL_MAP_STREAM_ABSENT",
                    format!(
                        "requested stream {requested:?} is absent from the complete in-scope group_key population"
                    ),
                    "inspect the exact group values in the requested source-event window and supply two values that physically exist",
                ));
            }
        }
        let mut selected = vec![a.clone(), b.clone()];
        selected.sort();
        return Ok(selected);
    }
    if by_group.len() < 2 {
        return Err(causal_error(
            "SYNAPSE_CALYX_CAUSAL_MAP_STREAMS_INSUFFICIENT",
            format!(
                "the requested scope contains {} distinct stream(s); a causal association requires at least two",
                by_group.len()
            ),
            "widen the source-event-time scope or choose a group_key with at least two physically observed values",
        ));
    }
    Ok(by_group.keys().cloned().collect())
}

fn validate_fdr_alpha(alpha: f32) -> Result<(), SynapseCalyxError> {
    if alpha.is_finite() && alpha > 0.0 && alpha < 1.0 {
        Ok(())
    } else {
        Err(causal_error(
            "SYNAPSE_CALYX_CAUSAL_MAP_FDR_ALPHA_INVALID",
            format!("causal_fdr_alpha must be finite and strictly inside (0,1); got {alpha}"),
            "supply a false-discovery-rate threshold such as 0.05",
        ))
    }
}

fn validate_bin_seconds(bin: f64) -> Result<f64, SynapseCalyxError> {
    if bin.is_finite() && bin > 0.0 {
        Ok(bin)
    } else {
        Err(causal_error(
            "SYNAPSE_CALYX_TEMPORAL_BIN_INVALID",
            "bin_seconds must be finite and positive",
            "supply a positive occurrence-count bin width in seconds",
        ))
    }
}

type AlignedBinnedStreams = (f64, usize, BTreeMap<String, Vec<f32>>);

fn aligned_binned_streams(
    selected: &[String],
    by_group: &BTreeMap<String, Vec<f64>>,
    bin: f64,
) -> Result<AlignedBinnedStreams, SynapseCalyxError> {
    let mut all = selected
        .iter()
        .flat_map(|name| by_group[name].iter().copied());
    let first = all.next().ok_or_else(|| {
        causal_error(
            "SYNAPSE_CALYX_CAUSAL_MAP_EMPTY_SCOPE",
            "the selected streams contain no physical events",
            "select streams with at least one source event",
        )
    })?;
    let (mut min_time, mut max_time) = (first, first);
    for time in all {
        min_time = min_time.min(time);
        max_time = max_time.max(time);
    }
    if !min_time.is_finite() || !max_time.is_finite() || max_time < min_time {
        return Err(causal_error(
            "SYNAPSE_CALYX_CAUSAL_MAP_TIME_RANGE_INVALID",
            "the selected event range is non-finite or reversed",
            "inspect and repair the persisted source-event timestamp metadata",
        ));
    }
    let max_index = temporal_bin_index((max_time - min_time) / bin)?;
    let n_bins = max_index.checked_add(1).ok_or_else(|| {
        causal_error(
            "SYNAPSE_CALYX_CAUSAL_MAP_BIN_COUNT_OVERFLOW",
            "the aligned causal-map bin count overflowed usize",
            "increase bin_seconds or narrow the source-event-time window",
        )
    })?;
    if n_bins > SYNAPSE_TEMPORAL_MAX_BINS {
        return Err(causal_error(
            "SYNAPSE_CALYX_CAUSAL_MAP_BIN_LIMIT_EXCEEDED",
            format!(
                "the aligned causal-map timeline requires {n_bins} bins, exceeding {SYNAPSE_TEMPORAL_MAX_BINS}"
            ),
            "increase bin_seconds or narrow since_ts_ns/until_ts_ns; the full timeline is required and will not be sampled",
        ));
    }
    let mut binned = BTreeMap::new();
    for name in selected {
        let mut counts = vec![0.0_f32; n_bins];
        for time in &by_group[name] {
            let index = temporal_bin_index((*time - min_time) / bin)?;
            let value = counts.get_mut(index).ok_or_else(|| {
                causal_error(
                    "SYNAPSE_CALYX_CAUSAL_MAP_BIN_INDEX_INVALID",
                    "an event mapped outside the validated aligned timeline",
                    "inspect source-event timestamps and bin_seconds",
                )
            })?;
            *value += 1.0;
        }
        binned.insert(name.clone(), counts);
    }
    Ok((min_time, n_bins, binned))
}

fn temporal_bin_index(value: f64) -> Result<usize, SynapseCalyxError> {
    if !value.is_finite() || value < 0.0 {
        return Err(causal_error(
            "SYNAPSE_CALYX_CAUSAL_MAP_BIN_INDEX_INVALID",
            "a causal-map bin index is negative, non-finite, or outside usize",
            "increase bin_seconds or repair the persisted source-event timestamp",
        ));
    }
    value.floor().to_usize().ok_or_else(|| {
        causal_error(
            "SYNAPSE_CALYX_CAUSAL_MAP_BIN_INDEX_INVALID",
            "a causal-map bin index does not fit usize",
            "increase bin_seconds or narrow the source-event-time window",
        )
    })
}

fn transfer_entropy_evidence(
    a: &[f32],
    b: &[f32],
    lags: &[usize],
) -> Result<SynapseCalyxCausalEstimatorEvidence, SynapseCalyxError> {
    let a_stream = a
        .iter()
        .copied()
        .enumerate()
        .map(|(i, v)| (i as u64, v))
        .collect::<Vec<_>>();
    let b_stream = b
        .iter()
        .copied()
        .enumerate()
        .map(|(i, v)| (i as u64, v))
        .collect::<Vec<_>>();
    // The causal map is content-addressed. A wall-clock timestamp inside one
    // estimator result would make identical source rows produce different
    // Graph keys, so the estimator runs under a fixed clock and its runtime-only
    // timestamp is removed from the persisted semantic projection below.
    let results = transfer_entropy_sweep(&a_stream, &b_stream, lags, &FixedClock::new(0));
    let measured = results
        .iter()
        .any(|result| !result.provisional && result.error_code.is_none());
    let mut result = json!({ "lags": results });
    canonicalize_transfer_entropy_projection(&mut result)?;
    let evidence = if measured {
        measured_evidence(
            "transfer_entropy_lag_sweep",
            vec![
                "directed predictive information, not structural effect".to_owned(),
                "stationary-enough aligned count process at each tested lag".to_owned(),
                "Gaussian equivalence means Granger and TE are not independent confirmations when that assumption holds".to_owned(),
            ],
            result,
        )
    } else {
        unresolved_evidence(
            "transfer_entropy_lag_sweep",
            vec!["directed predictive information, not structural effect".to_owned()],
            result,
            "CALYX_TE_UNRESOLVED",
            "no transfer-entropy lag reached a non-provisional estimate; inspect each persisted lag error_code and n_samples",
            "capture more aligned bins or correct the specific estimator failure; no other causal lane substitutes for transfer entropy",
        )
    };
    Ok(evidence)
}

fn canonicalize_transfer_entropy_projection(result: &mut Value) -> Result<(), SynapseCalyxError> {
    let lags = result
        .get_mut("lags")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| {
            causal_error(
                "SYNAPSE_CALYX_CAUSAL_MAP_CANONICALIZATION_FAILED",
                "the transfer-entropy projection has no lags array",
                "inspect the calyx-assay TEResult serialization contract and update the explicit causal-map projection",
            )
        })?;
    for (index, lag) in lags.iter_mut().enumerate() {
        let fields = lag.as_object_mut().ok_or_else(|| {
            causal_error(
                "SYNAPSE_CALYX_CAUSAL_MAP_CANONICALIZATION_FAILED",
                format!("transfer-entropy lag {index} is not an object"),
                "inspect the calyx-assay TEResult serialization contract and update the explicit causal-map projection",
            )
        })?;
        match fields.remove("computed_at") {
            Some(Value::Number(_)) => {}
            Some(_) => {
                return Err(causal_error(
                    "SYNAPSE_CALYX_CAUSAL_MAP_CANONICALIZATION_FAILED",
                    format!("transfer-entropy lag {index} has a non-numeric computed_at field"),
                    "restore TEResult.computed_at to its declared timestamp type or update the explicit causal-map projection",
                ));
            }
            None => {
                return Err(causal_error(
                    "SYNAPSE_CALYX_CAUSAL_MAP_CANONICALIZATION_FAILED",
                    format!("transfer-entropy lag {index} has no computed_at field"),
                    "inspect the calyx-assay TEResult serialization contract and update the explicit causal-map projection",
                ));
            }
        }
    }
    Ok(())
}

fn granger_evidence(
    source: &str,
    target: &str,
    x: &[f32],
    y: &[f32],
    lags: &[usize],
    hypotheses: &mut Vec<Hypothesis>,
) -> SynapseCalyxCausalEstimatorEvidence {
    let mut measured = Vec::new();
    let mut failures = Vec::new();
    for &lag in lags {
        match granger_causality_lags(x, y, lag) {
            Ok(report) => {
                hypotheses.push(Hypothesis {
                    id: format!("{source}->{target}@lag={lag}"),
                    p_value: report.p_value,
                });
                measured.push(report);
            }
            Err(error) => failures.push(estimator_error(&error)),
        }
    }
    let result =
        json!({ "source": source, "target": target, "tests": measured, "failures": failures });
    if result["tests"]
        .as_array()
        .is_some_and(|items| !items.is_empty())
    {
        measured_evidence(
            "linear_granger_f_test",
            vec![
                "predictive (Wiener-Granger) direction, not structural effect".to_owned(),
                "linear autoregression with finite paired samples".to_owned(),
                "stationary-enough residual process and no omitted driver interpretation"
                    .to_owned(),
            ],
            result,
        )
    } else {
        unresolved_evidence(
            "linear_granger_f_test",
            vec!["predictive direction under a linear autoregressive model".to_owned()],
            result,
            "CALYX_GRANGER_UNRESOLVED",
            "every requested lag failed the Granger estimator",
            "inspect the persisted per-lag failure codes; add samples or remove physical degeneracy/collinearity rather than substituting another estimator",
        )
    }
}

fn cross_correlation_evidence(
    a_name: &str,
    b_name: &str,
    a: &[f32],
    b: &[f32],
    max_lag: usize,
    hypotheses: &mut Vec<Hypothesis>,
) -> SynapseCalyxCausalEstimatorEvidence {
    match cross_correlation_profile(a, b, max_lag) {
        Ok(report) => {
            for point in &report.points {
                hypotheses.push(Hypothesis {
                    id: format!("{a_name}<->{b_name}@lag={}", point.lag),
                    p_value: point.p_value,
                });
            }
            measured_serializable_evidence(
                "signed_cross_correlation_profile",
                vec![
                    "descriptive lead/lag association only".to_owned(),
                    "positive lag means the first stream leads the second".to_owned(),
                ],
                &report,
            )
        }
        Err(error) => failed_evidence(
            "signed_cross_correlation_profile",
            vec!["descriptive lead/lag association only".to_owned()],
            &error,
        ),
    }
}

fn ccm_evidence(
    a_name: &str,
    b_name: &str,
    a: &[f32],
    b: &[f32],
) -> SynapseCalyxCausalEstimatorEvidence {
    let effective_points = a.len().saturating_sub(2);
    if effective_points <= 5 {
        return unresolved_evidence(
            "convergent_cross_mapping_simplex",
            vec!["nonlinear deterministic-ish scalar dynamical system".to_owned()],
            json!({ "effective_points": effective_points }),
            "CALYX_CCM_INSUFFICIENT_EFFECTIVE_POINTS",
            "the aligned series is too short to form two increasing CCM libraries larger than the neighbor count",
            "capture a longer aligned timeline; CCM cannot be replaced by a linear estimator",
        );
    }
    let small = (effective_points / 2).max(5).min(effective_points - 1);
    let config = CcmConfig::new(
        3,
        1,
        vec![small, effective_points],
        calyx_assay::DEFAULT_CCM_MIN_CONVERGENCE_DELTA,
        calyx_assay::DEFAULT_CCM_MIN_SKILL_GAP,
    );
    match calyx_assay::convergent_cross_mapping(a_name, a, b_name, b, &config) {
        Ok(report) => measured_serializable_evidence(
            "convergent_cross_mapping_simplex",
            vec![
                "nonlinear deterministic-ish scalar dynamical system".to_owned(),
                "cross-map skill must converge as library size grows".to_owned(),
                "not a general nonlinear causal-discovery guarantee".to_owned(),
            ],
            &report,
        ),
        Err(error) => failed_evidence(
            "convergent_cross_mapping_simplex",
            vec!["nonlinear deterministic-ish scalar dynamical system".to_owned()],
            &error,
        ),
    }
}

#[allow(clippy::cast_precision_loss)]
fn cross_k_evidence(
    a_name: &str,
    b_name: &str,
    a: &[f64],
    b: &[f64],
    origin: f64,
    bin_seconds: f64,
    max_lag: usize,
) -> SynapseCalyxCausalEstimatorEvidence {
    let a_relative = a.iter().map(|time| *time - origin).collect::<Vec<_>>();
    let b_relative = b.iter().map(|time| *time - origin).collect::<Vec<_>>();
    let max_event = a_relative
        .iter()
        .chain(&b_relative)
        .copied()
        .fold(0.0_f64, f64::max);
    let observation_end = max_event + bin_seconds.max(f64::EPSILON);
    let radii = (1..=max_lag)
        .map(|lag| bin_seconds * lag as f64)
        .filter(|radius| *radius <= observation_end)
        .collect::<Vec<_>>();
    if radii.is_empty() {
        return unresolved_evidence(
            "temporal_cross_type_ripley_k",
            vec!["descriptive event co-intensity; no edge correction".to_owned()],
            json!({ "a": a_name, "b": b_name }),
            "CALYX_CROSS_K_RADIUS_EMPTY",
            "no requested lag radius fits the physical observation window",
            "use a smaller bin_seconds/max_lag or widen the source-event window",
        );
    }
    match temporal_cross_k(&a_relative, &b_relative, &radii, 0.0, observation_end) {
        Ok(report) => measured_serializable_evidence(
            "temporal_cross_type_ripley_k",
            vec![
                "descriptive co-intensity, not causal direction".to_owned(),
                "no boundary correction; interpretation is limited to the declared window/radii"
                    .to_owned(),
            ],
            &report,
        ),
        Err(error) => failed_evidence(
            "temporal_cross_type_ripley_k",
            vec!["descriptive co-intensity, not causal direction".to_owned()],
            &error,
        ),
    }
}

fn pc_evidence(
    selected: &[String],
    binned: &BTreeMap<String, Vec<f32>>,
    alpha: f32,
) -> (SynapseCalyxCausalEstimatorEvidence, Vec<Hypothesis>) {
    let series = selected
        .iter()
        .map(|name| PcSeries {
            name,
            values: &binned[name],
        })
        .collect::<Vec<_>>();
    let depth = SYNAPSE_CAUSAL_MAP_PC_MAX_CONDITIONING.min(selected.len().saturating_sub(2));
    match pc_stable_gaussian(&series, alpha, depth) {
        Ok(report) => {
            let hypotheses = report
                .removed_edges
                .iter()
                .map(|edge| Hypothesis {
                    id: format!(
                        "{}<->{}|{}",
                        edge.left,
                        edge.right,
                        edge.conditioning_set.join(",")
                    ),
                    p_value: edge.p_value,
                })
                .collect();
            (
                measured_serializable_evidence(
                    "gaussian_pc_stable_skeleton",
                    vec![
                        "linear Gaussian conditional-independence tests".to_owned(),
                        "causal Markov, faithfulness, causal sufficiency, and sparse-DAG interpretation".to_owned(),
                        "skeleton only: retained edges are deliberately not oriented".to_owned(),
                        format!("maximum conditioning depth is explicitly bounded at {depth}"),
                    ],
                    &report,
                ),
                hypotheses,
            )
        }
        Err(error) => (
            failed_evidence(
                "gaussian_pc_stable_skeleton",
                vec![
                    "linear Gaussian conditional-independence tests".to_owned(),
                    "skeleton only; no invented orientation".to_owned(),
                ],
                &error,
            ),
            Vec::new(),
        ),
    }
}

fn partial_network_evidence(
    selected: &[String],
    binned: &BTreeMap<String, Vec<f32>>,
    alpha: f32,
) -> (SynapseCalyxCausalEstimatorEvidence, Vec<Hypothesis>) {
    let series = selected
        .iter()
        .map(|name| PartialNetworkSeries {
            name,
            values: &binned[name],
        })
        .collect::<Vec<_>>();
    match partial_correlation_network(
        &series,
        alpha,
        calyx_assay::DEFAULT_PARTIAL_NETWORK_MIN_ABS_R,
    ) {
        Ok(report) => {
            let hypotheses = report
                .retained_edges
                .iter()
                .map(|edge| (edge.left.as_str(), edge.right.as_str(), edge.p_value))
                .chain(
                    report
                        .pruned_edges
                        .iter()
                        .map(|edge| (edge.left.as_str(), edge.right.as_str(), edge.p_value)),
                )
                .map(|(left, right, p_value)| Hypothesis {
                    id: format!("{left}<->{right}|all_other_streams"),
                    p_value,
                })
                .collect();
            (
                measured_serializable_evidence(
                    "gaussian_partial_correlation_network",
                    vec![
                        "undirected conditional association, not causal orientation".to_owned(),
                        "linear Gaussian precision-matrix model".to_owned(),
                    ],
                    &report,
                ),
                hypotheses,
            )
        }
        Err(error) => (
            failed_evidence(
                "gaussian_partial_correlation_network",
                vec!["undirected linear Gaussian conditional association".to_owned()],
                &error,
            ),
            Vec::new(),
        ),
    }
}

#[allow(clippy::cast_possible_truncation)]
fn hawkes_evidence(
    selected: &[String],
    by_group: &BTreeMap<String, Vec<f64>>,
    origin: f64,
    bin_seconds: f64,
) -> SynapseCalyxCausalEstimatorEvidence {
    let relative = selected
        .iter()
        .map(|name| {
            by_group[name]
                .iter()
                .map(|time| (*time - origin) as f32)
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    let max_event = relative
        .iter()
        .flat_map(|times| times.iter().copied())
        .fold(0.0_f32, f32::max);
    let observation_end = max_event + (bin_seconds as f32).max(f32::EPSILON * max_event.max(1.0));
    let processes = selected
        .iter()
        .zip(&relative)
        .map(|(name, event_times)| HawkesEventSeries { name, event_times })
        .collect::<Vec<_>>();
    let config = HawkesConfig::new(
        observation_end,
        calyx_assay::DEFAULT_HAWKES_DECAY,
        calyx_assay::DEFAULT_HAWKES_ITERATIONS,
        calyx_assay::DEFAULT_HAWKES_MIN_EDGE_BRANCHING_RATIO,
    );
    match exponential_hawkes_em(&processes, &config) {
        Ok(report) => measured_serializable_evidence(
            "fixed_decay_exponential_hawkes_em",
            vec![
                "mutually exciting point processes with fixed exponential decay".to_owned(),
                "branching edges are event-triggering / Granger evidence, not an intervention effect".to_owned(),
                "stationary interpretation requires spectral radius below one".to_owned(),
            ],
            &report,
        ),
        Err(error) => failed_evidence(
            "fixed_decay_exponential_hawkes_em",
            vec!["fixed-decay mutually exciting point process".to_owned()],
            &error,
        ),
    }
}

fn fdr_family(
    name: &str,
    alpha: f32,
    hypotheses: Vec<Hypothesis>,
) -> Result<SynapseCalyxFdrFamily, SynapseCalyxError> {
    let q_values = benjamini_hochberg(
        &hypotheses
            .iter()
            .map(|hypothesis| hypothesis.p_value)
            .collect::<Vec<_>>(),
    )?;
    let decisions = hypotheses
        .into_iter()
        .zip(q_values)
        .map(|(hypothesis, q_value)| SynapseCalyxFdrDecision {
            hypothesis: hypothesis.id,
            p_value: hypothesis.p_value,
            q_value,
            significant: q_value <= alpha,
        })
        .collect::<Vec<_>>();
    Ok(SynapseCalyxFdrFamily {
        name: name.to_owned(),
        method: "benjamini_hochberg".to_owned(),
        assumptions: vec![
            "FDR control assumes independent or positive-regression-dependent p-values within this named family".to_owned(),
            "multiplicity is controlled within this family only; no cross-family error-rate claim is made".to_owned(),
        ],
        alpha,
        hypotheses_tested: decisions.len(),
        decisions,
    })
}

#[allow(clippy::cast_precision_loss)]
fn benjamini_hochberg(p_values: &[f32]) -> Result<Vec<f32>, SynapseCalyxError> {
    for (index, p_value) in p_values.iter().enumerate() {
        if !p_value.is_finite() || !(0.0..=1.0).contains(p_value) {
            return Err(causal_error(
                "SYNAPSE_CALYX_CAUSAL_MAP_P_VALUE_INVALID",
                format!("p-value family member {index} is outside finite [0,1]: {p_value}"),
                "inspect the named estimator's numerical invariants; invalid p-values cannot enter multiplicity correction",
            ));
        }
    }
    let m = p_values.len();
    if m == 0 {
        return Ok(Vec::new());
    }
    let mut order = (0..m).collect::<Vec<_>>();
    order.sort_by(|left, right| {
        p_values[*left]
            .total_cmp(&p_values[*right])
            .then_with(|| left.cmp(right))
    });
    let mut adjusted = vec![1.0_f32; m];
    let mut running = 1.0_f32;
    for rank_index in (0..m).rev() {
        let original = order[rank_index];
        let rank = rank_index + 1;
        let candidate = (p_values[original] * m as f32 / rank as f32).min(1.0);
        running = running.min(candidate);
        adjusted[original] = running;
    }
    Ok(adjusted)
}

fn measured_evidence(
    estimator: &str,
    assumptions: Vec<String>,
    result: Value,
) -> SynapseCalyxCausalEstimatorEvidence {
    SynapseCalyxCausalEstimatorEvidence {
        estimator: estimator.to_owned(),
        status: "measured".to_owned(),
        assumptions,
        result: Some(result),
        error: None,
    }
}

fn measured_serializable_evidence<T: Serialize>(
    estimator: &str,
    assumptions: Vec<String>,
    result: &T,
) -> SynapseCalyxCausalEstimatorEvidence {
    match serde_json::to_value(result) {
        Ok(value) => measured_evidence(estimator, assumptions, value),
        Err(error) => unresolved_evidence(
            estimator,
            assumptions,
            json!({ "serialization_error": error.to_string() }),
            "SYNAPSE_CALYX_CAUSAL_ESTIMATOR_ENCODE_FAILED",
            "the estimator produced a report that could not be encoded into the causal-map artifact",
            "inspect the estimator report for a non-finite or unsupported field and repair its serialization contract",
        ),
    }
}

fn unresolved_evidence(
    estimator: &str,
    assumptions: Vec<String>,
    result: Value,
    code: &str,
    message: &str,
    remediation: &str,
) -> SynapseCalyxCausalEstimatorEvidence {
    tracing::warn!(
        code,
        estimator,
        message,
        remediation,
        "causal estimator did not produce a measured lane"
    );
    SynapseCalyxCausalEstimatorEvidence {
        estimator: estimator.to_owned(),
        status: "unresolved".to_owned(),
        assumptions,
        result: Some(result),
        error: Some(SynapseCalyxCausalEstimatorError {
            code: code.to_owned(),
            message: message.to_owned(),
            remediation: remediation.to_owned(),
        }),
    }
}

fn failed_evidence(
    estimator: &str,
    assumptions: Vec<String>,
    error: &calyx_core::CalyxError,
) -> SynapseCalyxCausalEstimatorEvidence {
    tracing::warn!(
        code = error.code,
        estimator,
        error = %error,
        remediation = error.remediation,
        "causal estimator failed closed; retaining the typed failure in the complete map"
    );
    SynapseCalyxCausalEstimatorEvidence {
        estimator: estimator.to_owned(),
        status: "failed".to_owned(),
        assumptions,
        result: None,
        error: Some(estimator_error(error)),
    }
}

fn estimator_error(error: &calyx_core::CalyxError) -> SynapseCalyxCausalEstimatorError {
    SynapseCalyxCausalEstimatorError {
        code: error.code.to_owned(),
        message: error.to_string(),
        remediation: error.remediation.to_owned(),
    }
}

fn causal_map_key(panel_version: u32, digest: &[u8]) -> Vec<u8> {
    let mut key = Vec::with_capacity(GRAPH_CAUSAL_MAP_PREFIX.len() + 4 + 16);
    key.extend_from_slice(GRAPH_CAUSAL_MAP_PREFIX);
    key.extend_from_slice(&panel_version.to_be_bytes());
    key.extend_from_slice(&digest[..16]);
    key
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from(HEX[usize::from(byte >> 4)]));
        out.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    out
}

fn causal_error(
    code: &'static str,
    message: impl Into<String>,
    remediation: &'static str,
) -> SynapseCalyxError {
    SynapseCalyxError::new(code, message.into(), remediation)
}
