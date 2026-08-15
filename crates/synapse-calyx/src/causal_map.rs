//! Exhaustive, typed causal-evidence maps over temporal Calyx constellations.
//!
//! This module deliberately does not manufacture one universal "causal score".
//! Every estimator answers a different question under different assumptions, so
//! the persisted artifact keeps conditional association, directed predictive
//! information, nonlinear state-space evidence, event co-intensity, and Hawkes
//! triggering in separate lanes. Observational estimator success never upgrades
//! the artifact to an identified structural causal effect.

use std::collections::BTreeMap;

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
    pub graph_cf_rows_after: usize,
    pub physical_readback_matches: bool,
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
        validate_fdr_alpha(fdr_alpha)?;
        let group_key = required_trimmed_group_key(params)?;
        let pair_scope = requested_pair_scope(params)?;
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
        let bin_seconds = validate_bin_seconds(params.bin_seconds)?;
        let max_lag = params.max_lag.clamp(1, SYNAPSE_TEMPORAL_MAX_LAGS);
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
        let artifact = SynapseCalyxCausalMapArtifact {
            schema: "synapse.calyx.causal_map.v1".to_owned(),
            panel_version: params.panel_version,
            group_key: group_key.to_owned(),
            pair_scope: if pair_scope.is_some() { "explicit_pair" } else { "complete" }.to_owned(),
            since_ts_ns: params.since_ts_ns,
            until_ts_ns: params.until_ts_ns,
            max_records: params.max_records.clamp(1, crate::SYNAPSE_INTELLIGENCE_MAX_RECORDS),
            source_records: records.len(),
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
        self.write_cf_batch(vec![SynapseCalyxCfWrite {
            cf: ColumnFamily::Graph,
            key: graph_key.clone(),
            value: bytes.clone(),
        }])?;
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
        let graph_cf_rows_after = self
            .count_cf_latest_bounded_memoized(ColumnFamily::Graph)?
            .rows();
        let graph_key_hex = hex_encode(&graph_key);
        let graph_value_sha256 = hex_encode(&digest);
        tracing::info!(
            code = "SYNAPSE_CALYX_CAUSAL_MAP_PERSISTED_AND_READ_BACK",
            panel_version = params.panel_version,
            source_records = artifact.source_records,
            stream_count = artifact.streams.len(),
            pair_count = artifact.pairs.len(),
            graph_key_hex,
            graph_value_sha256,
            graph_value_bytes = bytes.len(),
            graph_cf_rows_after,
            "completed exhaustive causal map and byte-exact physical readback"
        );
        Ok(SynapseCalyxCausalMapReport {
            artifact,
            graph_key_hex,
            graph_value_sha256,
            graph_value_bytes: bytes.len(),
            graph_cf_rows_after,
            physical_readback_matches: true,
        })
    }
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
        (Some(a), Some(b)) if !a.trim().is_empty() && !b.trim().is_empty() && a != b => {
            Ok(Some((a.clone(), b.clone())))
        }
        (Some(a), Some(b)) if a == b => Err(causal_error(
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
