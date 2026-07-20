use std::collections::BTreeSet;

use calyx_aster::vault::AsterVault;
use calyx_core::{Clock, CxId, content_address};
use calyx_mincut::{betweenness_auto, tarjan_scc};
use calyx_paths::AssocGraph;
use serde::{Deserialize, Serialize};

use crate::grounding_gaps::{CALYX_KERNEL_EMPTY, grounding_gaps_for_members};
use crate::recall_eval::RecallEvalParams;
use crate::temporal_kernel::apply_frequency_bonuses;
use crate::{
    DfvsResult, KernelGraph, KernelGraphParams, LpRoundParams, Result, dfvs_approx,
    select_kernel_graph,
};

/// Graphs with at most this many nodes use exact Brandes betweenness; larger
/// graphs switch to the sampled estimator. Exact betweenness is O(V·(E+V·log V)),
/// so unbounded use is intractable on the 10^5-node corpus graph.
const BETWEENNESS_EXACT_MAX_NODES: usize = 2_000;
/// Pivot count for sampled betweenness on large graphs (cost O(k·(E+V·log V))).
const BETWEENNESS_PIVOTS: usize = 512;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GroundednessReport {
    pub reached_anchor: f32,
    pub unanchored_members: Vec<CxId>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecallReport {
    pub kernel_only: f32,
    pub full: f32,
    pub ratio: f32,
    pub approx_factor: f64,
    pub tau_star_estimate: usize,
    pub tau_star_exact: bool,
    pub recall_eval_params: Option<RecallEvalParams>,
    pub corpus_name: Option<String>,
    pub n_queries_tested: usize,
    pub held_out: Vec<CxId>,
    pub warning: Option<String>,
}

impl Default for RecallReport {
    fn default() -> Self {
        Self {
            kernel_only: 0.0,
            full: 0.0,
            ratio: 0.0,
            approx_factor: 1.0,
            tau_star_estimate: 0,
            tau_star_exact: true,
            recall_eval_params: None,
            corpus_name: None,
            n_queries_tested: 0,
            held_out: Vec::new(),
            warning: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Kernel {
    pub kernel_id: CxId,
    pub panel_version: u32,
    pub anchor_kind: Option<String>,
    pub corpus_shard_hash: [u8; 32],
    pub members: Vec<CxId>,
    pub kernel_graph: Vec<CxId>,
    pub groundedness: GroundednessReport,
    pub recall: RecallReport,
    pub built_at_millis: u64,
    pub estimator_provenance: String,
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct KernelParams {
    pub panel_version: u32,
    pub anchor_kind: Option<String>,
    pub corpus_shard_hash: [u8; 32],
    pub built_at_millis: u64,
    pub kernel_graph: KernelGraphParams,
    pub lp_round: LpRoundParams,
}

impl Default for KernelParams {
    fn default() -> Self {
        Self {
            panel_version: 1,
            anchor_kind: Some("synthetic".to_string()),
            corpus_shard_hash: [0; 32],
            built_at_millis: 0,
            kernel_graph: KernelGraphParams::default(),
            lp_round: LpRoundParams::default(),
        }
    }
}

pub fn build_kernel_pipeline(
    graph: &AssocGraph,
    anchors: &[CxId],
    params: &KernelParams,
) -> Result<Kernel> {
    build_kernel_pipeline_with_adjustment(graph, anchors, params, |_| Ok(()))
}

pub fn build_kernel_pipeline_with_frequency<C>(
    graph: &AssocGraph,
    anchors: &[CxId],
    params: &KernelParams,
    vault: &AsterVault<C>,
) -> Result<Kernel>
where
    C: Clock,
{
    build_kernel_pipeline_with_adjustment(graph, anchors, params, |heuristic| {
        apply_frequency_bonuses(heuristic, graph, vault).map(|_| ())
    })
}

pub fn refine_kernel_with_recall_support(
    mut kernel: Kernel,
    support_members: &[CxId],
    graph: &AssocGraph,
    anchors: &[CxId],
    params: &KernelParams,
    reason: &str,
) -> Result<Kernel> {
    let before_members = kernel.members.len();
    let before_graph = kernel.kernel_graph.len();
    let mut members = kernel.members.iter().copied().collect::<BTreeSet<_>>();
    let mut kernel_graph = kernel.kernel_graph.iter().copied().collect::<BTreeSet<_>>();
    for member in support_members {
        graph.require_node_index(*member)?;
        members.insert(*member);
        kernel_graph.insert(*member);
    }
    kernel.members = members.into_iter().collect();
    kernel.kernel_graph = kernel_graph.into_iter().collect();
    let gap_report = grounding_gaps_for_members(
        &kernel.members,
        graph,
        anchors,
        params.kernel_graph.max_groundedness_distance,
    )?;
    kernel.groundedness = groundedness_report(&kernel.members, gap_report.gaps);
    kernel.kernel_id = kernel_id(params, &kernel.members, &kernel.kernel_graph);
    kernel.warnings.push(format!(
        "CALYX_KERNEL_RECALL_REFINED: reason={reason}; support_members={}; members_before={before_members}; members_after={}; kernel_graph_before={before_graph}; kernel_graph_after={}",
        support_members.len(),
        kernel.members.len(),
        kernel.kernel_graph.len()
    ));
    kernel.estimator_provenance = format!(
        "{}; recall_refinement={reason}; recall_members_added={}; recall_kernel_graph_added={}",
        kernel.estimator_provenance,
        kernel.members.len().saturating_sub(before_members),
        kernel.kernel_graph.len().saturating_sub(before_graph)
    );
    Ok(kernel)
}

/// Re-seals a completed kernel after recall and build-policy fields are final.
///
/// The pipeline's provisional id is useful while constructing its index, but
/// it intentionally predates the measured recall report. Persisted generations
/// must instead include every semantic output plus the caller's physical graph
/// contract so two different builds can never overwrite the same id directory.
pub fn seal_completed_kernel_identity(
    kernel: &mut Kernel,
    physical_contract_hash: &[u8; 32],
) -> Result<CxId> {
    let mut identity = kernel.clone();
    identity.kernel_id = CxId::from_bytes([0; 16]);
    let bytes = serde_json::to_vec(&identity).map_err(|error| {
        crate::LodestarError::KernelArtifactCodec {
            detail: format!("encode completed kernel identity: {error}"),
        }
    })?;
    let id = CxId::from_bytes(content_address([
        b"calyx-lodestar-completed-kernel-v1".as_slice(),
        physical_contract_hash.as_slice(),
        bytes.as_slice(),
    ]));
    kernel.kernel_id = id;
    Ok(id)
}

fn build_kernel_pipeline_with_adjustment(
    graph: &AssocGraph,
    anchors: &[CxId],
    params: &KernelParams,
    mut adjust_heuristic: impl FnMut(&mut KernelGraph) -> Result<()>,
) -> Result<Kernel> {
    if graph.is_empty() {
        return Ok(empty_kernel(params));
    }
    let scc = tarjan_scc(graph);
    // Exact Brandes betweenness up to BETWEENNESS_EXACT_MAX_NODES; beyond that the
    // sampled estimator (BETWEENNESS_PIVOTS pivots) keeps corpus-scale graphs
    // (10^5+ nodes) tractable while preserving the centrality ranking used for
    // kernel selection.
    let bet = betweenness_auto(graph, BETWEENNESS_EXACT_MAX_NODES, BETWEENNESS_PIVOTS)?;
    let mut heuristic = select_kernel_graph(graph, &scc, &bet, anchors, &params.kernel_graph)?;
    adjust_heuristic(&mut heuristic)?;
    let candidate_graph = heuristic;
    let dfvs = dfvs_approx(&candidate_graph)?;
    let gap_report = grounding_gaps_for_members(
        &dfvs.members,
        graph,
        anchors,
        params.kernel_graph.max_groundedness_distance,
    )?;
    let warnings = warnings(&candidate_graph.warnings, &dfvs, &gap_report.gaps);
    let provenance = estimator_provenance(&dfvs, &warnings);
    let kernel_graph = candidate_graph.selected.clone();
    let kernel_id = kernel_id(params, &dfvs.members, &kernel_graph);

    Ok(Kernel {
        kernel_id,
        panel_version: params.panel_version,
        anchor_kind: params.anchor_kind.clone(),
        corpus_shard_hash: params.corpus_shard_hash,
        members: dfvs.members.clone(),
        kernel_graph,
        groundedness: groundedness_report(&dfvs.members, gap_report.gaps),
        recall: RecallReport {
            approx_factor: dfvs.approx_factor,
            tau_star_estimate: dfvs.tau_star_estimate,
            tau_star_exact: dfvs.tau_star_exact,
            ..RecallReport::default()
        },
        built_at_millis: params.built_at_millis,
        estimator_provenance: provenance,
        warnings,
    })
}

fn groundedness_report(members: &[CxId], unanchored: Vec<CxId>) -> GroundednessReport {
    let reached = members.len().saturating_sub(unanchored.len());
    GroundednessReport {
        reached_anchor: if members.is_empty() {
            0.0
        } else {
            reached as f32 / members.len() as f32
        },
        unanchored_members: unanchored,
    }
}

fn warnings(rounded_warnings: &[String], dfvs: &DfvsResult, unanchored: &[CxId]) -> Vec<String> {
    let mut warnings = rounded_warnings.to_vec();
    if dfvs.members.is_empty() {
        warnings.push(format!("{CALYX_KERNEL_EMPTY}: kernel has no members"));
    } else if unanchored.len() == dfvs.members.len() {
        warnings.push("CALYX_KERNEL_UNGROUNDED: all kernel members are provisional".to_string());
    }
    warnings
}

fn estimator_provenance(dfvs: &DfvsResult, warnings: &[String]) -> String {
    let trust = if warnings
        .iter()
        .any(|warning| warning.starts_with(CALYX_KERNEL_EMPTY))
    {
        "empty"
    } else if warnings
        .iter()
        .any(|warning| warning.starts_with("CALYX_KERNEL_UNGROUNDED"))
    {
        "provisional"
    } else {
        "anchored"
    };
    format!(
        "ph32::{:?}; approx_factor={:.6}; tau_star_estimate={}; tau_star_exact={}; trust={trust}",
        dfvs.method, dfvs.approx_factor, dfvs.tau_star_estimate, dfvs.tau_star_exact
    )
}

fn kernel_id(params: &KernelParams, members: &[CxId], kernel_graph: &[CxId]) -> CxId {
    let mut parts = vec![
        params.panel_version.to_be_bytes().to_vec(),
        params.anchor_kind.clone().unwrap_or_default().into_bytes(),
        params.corpus_shard_hash.to_vec(),
    ];
    parts.extend(members.iter().map(|id| id.as_bytes().to_vec()));
    parts.extend(kernel_graph.iter().map(|id| id.as_bytes().to_vec()));
    CxId::from_bytes(content_address(parts))
}

fn empty_kernel(params: &KernelParams) -> Kernel {
    Kernel {
        kernel_id: kernel_id(params, &[], &[]),
        panel_version: params.panel_version,
        anchor_kind: params.anchor_kind.clone(),
        corpus_shard_hash: params.corpus_shard_hash,
        members: Vec::new(),
        kernel_graph: Vec::new(),
        groundedness: GroundednessReport {
            reached_anchor: 0.0,
            unanchored_members: Vec::new(),
        },
        recall: RecallReport::default(),
        built_at_millis: params.built_at_millis,
        estimator_provenance: "ph32::empty; trust=empty".to_string(),
        warnings: vec![format!("{CALYX_KERNEL_EMPTY}: kernel has no members")],
    }
}
