//! Durable commissioning of the optimized search kernels.
//!
//! The production query paths are normally exercised only when a live panel
//! happens to contain the corresponding slot/index shape. This bounded surface
//! lets operators prove the exact CPU-reference PQ builder, live and persisted
//! `MaxSim` scorers, and SPANN centroid router before admitting those paths on a
//! host. Caller-supplied vectors are real inputs; the resulting artifact is
//! committed to Aster's KV CF and independently read back before success.

use calyx_aster::cf::ColumnFamily;
use calyx_search::score_persisted_maxsim_pair;
use calyx_sextant::index::{
    DiskAnnPqBuildExecution, DiskAnnPqBuildParams, DiskAnnPqIndex, MaxSimIndex, try_build_centroids,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{SynapseCalyxCfWrite, SynapseCalyxError, SynapseCalyxVault};

const COMMISSION_KEY_PREFIX: &[u8; 5] = b"SKCM1";
const MAX_ROWS: usize = 256;
const MAX_DIM: usize = 256;
const MAX_TOKENS: usize = 64;
const MAX_ITERATIONS: usize = 64;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SynapseCalyxSearchCommissionParams {
    pub panel_version: u32,
    pub rows: Vec<Vec<f32>>,
    pub query: Vec<f32>,
    pub pq_subvectors: usize,
    pub pq_centroids: usize,
    pub pq_iterations: usize,
    pub maxsim_query: Vec<Vec<f32>>,
    pub maxsim_document: Vec<Vec<f32>>,
    pub spann_clusters: usize,
    pub seed: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SynapseCalyxSearchCommissionArtifact {
    pub format: &'static str,
    pub panel_version: u32,
    pub input_sha256: String,
    pub row_count: usize,
    pub dim: usize,
    pub pq_backend: String,
    pub pq_codes: Vec<u8>,
    pub pq_codes_sha256: String,
    pub pq_codebook_sha256: String,
    pub pq_query_distances_bits: Vec<u32>,
    pub maxsim_live_bits: u32,
    pub maxsim_persisted_bits: u32,
    pub maxsim_bit_identical: bool,
    pub spann_centroids_sha256: String,
    pub spann_assignments: Vec<(u32, u32)>,
    pub spann_exact_query_order: Vec<u32>,
    pub spann_graph_query_order: Vec<u32>,
    pub spann_graph_matches_exact: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SynapseCalyxSearchCommissionReport {
    pub source_of_truth: &'static str,
    pub key_hex: String,
    pub committed_seq: u64,
    pub artifact_sha256: String,
    pub physical_readback_sha256: String,
    pub physical_readback_matches: bool,
    pub artifact: SynapseCalyxSearchCommissionArtifact,
}

impl SynapseCalyxVault {
    /// Runs and durably proves the bounded optimized search-kernel suite.
    ///
    /// # Errors
    ///
    /// Returns a structured error before persistence for invalid inputs or any
    /// kernel/parity failure, and after persistence if the exact KV bytes do not
    /// read back from Aster's serving view.
    pub fn commission_search_kernels(
        &self,
        params: &SynapseCalyxSearchCommissionParams,
    ) -> Result<SynapseCalyxSearchCommissionReport, SynapseCalyxError> {
        validate(params)?;
        let input_bytes = serde_json::to_vec(params).map_err(|error| {
            invalid(format!(
                "serialize search commissioning input canonically: {error}"
            ))
        })?;
        let input_sha256 = sha256_hex(&input_bytes);
        let rows = params
            .rows
            .iter()
            .cloned()
            .enumerate()
            .map(|(id, row)| {
                u32::try_from(id)
                    .map(|id| (id, row))
                    .map_err(|error| invalid(format!("commissioning row id overflow: {error}")))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let artifact = build_artifact(params, &rows, input_sha256)?;
        require_kernel_parity(&artifact)?;

        let artifact_bytes = serde_json::to_vec(&artifact).map_err(|error| {
            invalid(format!("serialize search commissioning artifact: {error}"))
        })?;
        let artifact_sha256 = sha256_hex(&artifact_bytes);
        let input_digest = Sha256::digest(&input_bytes);
        let mut key = Vec::with_capacity(COMMISSION_KEY_PREFIX.len() + 4 + input_digest.len());
        key.extend_from_slice(COMMISSION_KEY_PREFIX);
        key.extend_from_slice(&params.panel_version.to_be_bytes());
        key.extend_from_slice(&input_digest);
        let committed_seq = self.write_cf_batch(vec![SynapseCalyxCfWrite::new(
            ColumnFamily::Kv,
            key.clone(),
            artifact_bytes.clone(),
        )])?;
        self.flush()?;
        let readback = self
            .read_cf_latest(ColumnFamily::Kv, &key)?
            .ok_or_else(|| {
                SynapseCalyxError::new(
                    "SYNAPSE_CALYX_SEARCH_COMMISSION_READBACK_MISSING",
                    format!("committed search commissioning row disappeared at seq {committed_seq}"),
                    "inspect Aster WAL/MVCC publication and KV serving state; do not admit optimized search kernels until exact readback succeeds",
                )
            })?;
        let physical_readback_sha256 = sha256_hex(&readback);
        if readback != artifact_bytes {
            return Err(SynapseCalyxError::new(
                "SYNAPSE_CALYX_SEARCH_COMMISSION_READBACK_MISMATCH",
                format!(
                    "KV readback sha256 {physical_readback_sha256} != committed artifact sha256 {artifact_sha256}"
                ),
                "inspect Aster WAL/MVCC serialization and serving state; do not admit optimized search kernels until the committed bytes read back exactly",
            ));
        }
        Ok(SynapseCalyxSearchCommissionReport {
            source_of_truth: "Calyx Kv CF row",
            key_hex: hex(&key),
            committed_seq,
            artifact_sha256,
            physical_readback_sha256,
            physical_readback_matches: true,
            artifact,
        })
    }
}

fn build_artifact(
    params: &SynapseCalyxSearchCommissionParams,
    rows: &[(u32, Vec<f32>)],
    input_sha256: String,
) -> Result<SynapseCalyxSearchCommissionArtifact, SynapseCalyxError> {
    let pq = DiskAnnPqIndex::build_with_execution(
        rows,
        DiskAnnPqBuildParams {
            subvectors: params.pq_subvectors,
            centroids: params.pq_centroids,
            iterations: params.pq_iterations,
        },
        DiskAnnPqBuildExecution::CpuReference,
    )
    .map_err(|error| SynapseCalyxError::from_calyx("commission CPU-reference PQ", &error))?;
    let pq_query = pq
        .query(&params.query)
        .map_err(|error| SynapseCalyxError::from_calyx("commission PQ query", &error))?;
    let pq_query_distances_bits = rows
        .iter()
        .map(|(id, _)| {
            pq_query
                .distance_l2(*id)
                .map(f32::to_bits)
                .map_err(|error| {
                    SynapseCalyxError::from_calyx(
                        "read commissioned PQ approximate distance",
                        &error,
                    )
                })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let maxsim_live = MaxSimIndex::maxsim(&params.maxsim_query, &params.maxsim_document);
    let maxsim_persisted =
        score_persisted_maxsim_pair(&params.maxsim_query, &params.maxsim_document).map_err(
            |error| {
                let catalog: calyx_core::CalyxError = error.into();
                SynapseCalyxError::from_calyx("commission persisted MaxSim scorer", &catalog)
            },
        )?;
    let spann = try_build_centroids(rows, params.spann_clusters, params.seed)
        .map_err(|error| SynapseCalyxError::from_calyx("commission SPANN centroids", &error))?;
    let probe_count = spann.centroid_count();
    let spann_exact_query_order = spann.nearest_centroids_exact_l2(&params.query, probe_count);
    let spann_graph_query_order = spann.nearest_centroids_raw_l2_graph(&params.query, probe_count);
    Ok(SynapseCalyxSearchCommissionArtifact {
        format: "synapse-search-kernel-commission-v1",
        panel_version: params.panel_version,
        input_sha256,
        row_count: rows.len(),
        dim: params.query.len(),
        pq_backend: pq.build_diagnostics().backend.clone(),
        pq_codes: pq.codes().to_vec(),
        pq_codes_sha256: sha256_hex(pq.codes()),
        pq_codebook_sha256: sha256_f32(pq.codebook()),
        pq_query_distances_bits,
        maxsim_live_bits: maxsim_live.to_bits(),
        maxsim_persisted_bits: maxsim_persisted.to_bits(),
        maxsim_bit_identical: maxsim_live.to_bits() == maxsim_persisted.to_bits(),
        spann_centroids_sha256: sha256_f32_nested(spann.centroids()),
        spann_assignments: spann.assignments().to_vec(),
        spann_exact_query_order: spann_exact_query_order.clone(),
        spann_graph_query_order: spann_graph_query_order.clone(),
        spann_graph_matches_exact: spann_exact_query_order == spann_graph_query_order,
    })
}

fn require_kernel_parity(
    artifact: &SynapseCalyxSearchCommissionArtifact,
) -> Result<(), SynapseCalyxError> {
    if !artifact.maxsim_bit_identical {
        return Err(SynapseCalyxError::new(
            "SYNAPSE_CALYX_SEARCH_MAXSIM_PARITY_FAILED",
            format!(
                "live MaxSim bits {} != persisted MaxSim bits {}",
                artifact.maxsim_live_bits, artifact.maxsim_persisted_bits
            ),
            "do not admit multi-vector generations; inspect the live and persisted MaxSim kernel dispatch and restore bit-identical scoring",
        ));
    }
    if !artifact.spann_graph_matches_exact {
        return Err(SynapseCalyxError::new(
            "SYNAPSE_CALYX_SEARCH_SPANN_ROUTING_PARITY_FAILED",
            format!(
                "SPANN raw-L2 graph order {:?} != exact order {:?}",
                artifact.spann_graph_query_order, artifact.spann_exact_query_order
            ),
            "do not admit the SPANN generation; inspect centroid graph construction and raw-L2 routing until the bounded commissioning corpus matches exact order",
        ));
    }
    Ok(())
}

fn validate(params: &SynapseCalyxSearchCommissionParams) -> Result<(), SynapseCalyxError> {
    if params.panel_version == 0 {
        return Err(invalid(
            "search commissioning panel_version must be positive",
        ));
    }
    if params.rows.is_empty() || params.rows.len() > MAX_ROWS {
        return Err(invalid(format!(
            "search commissioning rows must contain 1..={MAX_ROWS} vectors; received {}",
            params.rows.len()
        )));
    }
    let dim = params.rows[0].len();
    if dim == 0 || dim > MAX_DIM {
        return Err(invalid(format!(
            "search commissioning dimension must be 1..={MAX_DIM}; received {dim}"
        )));
    }
    if params.query.len() != dim {
        return Err(invalid(format!(
            "search commissioning query dimension {} != row dimension {dim}",
            params.query.len()
        )));
    }
    if params
        .rows
        .iter()
        .any(|row| row.len() != dim || row.iter().any(|value| !value.is_finite()))
        || params.query.iter().any(|value| !value.is_finite())
    {
        return Err(invalid(
            "search commissioning rows/query contain non-finite or inconsistent vectors",
        ));
    }
    if params.pq_subvectors == 0
        || params.pq_centroids == 0
        || params.pq_centroids > 256
        || params.pq_iterations == 0
        || params.pq_iterations > MAX_ITERATIONS
        || params.spann_clusters == 0
        || params.spann_clusters > params.rows.len()
    {
        return Err(invalid(format!(
            "invalid bounded search commissioning parameters: pq_subvectors={} pq_centroids={} pq_iterations={} spann_clusters={}",
            params.pq_subvectors, params.pq_centroids, params.pq_iterations, params.spann_clusters
        )));
    }
    let valid_tokens = |tokens: &[Vec<f32>]| {
        let token_dim = tokens.first().map(Vec::len).unwrap_or_default();
        !tokens.is_empty()
            && tokens.len() <= MAX_TOKENS
            && token_dim > 0
            && token_dim <= MAX_DIM
            && tokens.iter().all(|token| {
                token.len() == token_dim && token.iter().all(|value| value.is_finite())
            })
    };
    if !valid_tokens(&params.maxsim_query)
        || !valid_tokens(&params.maxsim_document)
        || params.maxsim_query[0].len() != params.maxsim_document[0].len()
    {
        return Err(invalid(
            "MaxSim query/document must each have 1..=64 finite, consistent tokens with one shared dimension in 1..=256",
        ));
    }
    Ok(())
}

fn invalid(message: impl Into<String>) -> SynapseCalyxError {
    SynapseCalyxError::new(
        "SYNAPSE_CALYX_SEARCH_COMMISSION_INVALID",
        message,
        "supply a bounded finite commissioning corpus with consistent dimensions and positive PQ/SPANN parameters; no artifact was written",
    )
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

fn sha256_f32(values: &[f32]) -> String {
    let mut hasher = Sha256::new();
    for value in values {
        hasher.update(value.to_le_bytes());
    }
    hex(&hasher.finalize())
}

fn sha256_f32_nested(values: &[Vec<f32>]) -> String {
    let mut hasher = Sha256::new();
    for row in values {
        hasher.update((row.len() as u64).to_le_bytes());
        for value in row {
            hasher.update(value.to_le_bytes());
        }
    }
    hex(&hasher.finalize())
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(DIGITS[(byte >> 4) as usize] as char);
        out.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    out
}
