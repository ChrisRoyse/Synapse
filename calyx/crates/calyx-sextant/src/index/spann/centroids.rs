//! SPANN centroid state persisted as `centroids.spn`.

mod codec;
mod raw_l2_graph;

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{BufWriter, Write as _};
use std::path::{Path, PathBuf};

use calyx_core::{CxId, Result, SlotId, SlotVector};
use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha8Rng;
use rayon::prelude::*;

use crate::error::{
    CALYX_INDEX_CORRUPT, CALYX_INDEX_DIM_MISMATCH, CALYX_INDEX_INVALID_PARAMS, CALYX_INDEX_IO,
    sextant_error,
};
use crate::index::distance::l2_sq;
use crate::index::{HnswIndex, SextantIndex};
use codec::{decode_centroids, write_header};
use raw_l2_graph::RawL2CentroidGraph;

pub const SPANN_CENTROID_MAGIC: [u8; 8] = *b"CLXSP001";
const FORMAT_VERSION: u32 = 1;
const KMEANS_ITERS: usize = 12;
const RAW_L2_GRAPH_EF_FLOOR: usize = 128;
const CENTROID_SLOT: SlotId = SlotId::new(u16::MAX - 1);

#[derive(Clone, Debug)]
pub struct SpannCentroidIndex {
    dim: u32,
    centroids: Vec<Vec<f32>>,
    posting_list_offsets: Vec<u64>,
    assignments: Vec<(u32, u32)>,
    assignment_lookup: BTreeMap<u32, u32>,
    hnsw: HnswIndex,
    centroid_lookup: BTreeMap<CxId, u32>,
    raw_l2_graph: RawL2CentroidGraph,
}

impl SpannCentroidIndex {
    pub fn empty(dim: u32) -> Self {
        Self::from_parts(dim, Vec::new(), Vec::new(), Vec::new())
            .expect("empty centroid index is valid")
    }

    pub fn from_parts(
        dim: u32,
        centroids: Vec<Vec<f32>>,
        posting_list_offsets: Vec<u64>,
        assignments: Vec<(u32, u32)>,
    ) -> Result<Self> {
        validate_centroids(dim, &centroids)?;
        let mut offsets = posting_list_offsets;
        if offsets.is_empty() {
            offsets = (0..centroids.len() as u64).collect();
        }
        if offsets.len() != centroids.len() {
            return Err(invalid(format!(
                "posting offset count {} != centroid count {}",
                offsets.len(),
                centroids.len()
            )));
        }
        for &(_, centroid_id) in &assignments {
            if centroid_id as usize >= centroids.len() {
                return Err(invalid(format!(
                    "assignment references centroid {centroid_id} but count is {}",
                    centroids.len()
                )));
            }
        }
        let (hnsw, lookup) = build_hnsw(dim, &centroids)?;
        let raw_l2_graph = RawL2CentroidGraph::build(&centroids);
        let assignment_lookup = assignments.iter().copied().collect();
        Ok(Self {
            dim,
            centroids,
            posting_list_offsets: offsets,
            assignments,
            assignment_lookup,
            hnsw,
            centroid_lookup: lookup,
            raw_l2_graph,
        })
    }

    pub fn dim(&self) -> u32 {
        self.dim
    }

    pub fn centroid_count(&self) -> usize {
        self.centroids.len()
    }

    pub fn centroids(&self) -> &[Vec<f32>] {
        &self.centroids
    }

    pub fn posting_list_offsets(&self) -> &[u64] {
        &self.posting_list_offsets
    }

    pub fn assignments(&self) -> &[(u32, u32)] {
        &self.assignments
    }

    pub fn assignment(&self, vector_id: u32) -> Option<u32> {
        self.assignment_lookup.get(&vector_id).copied()
    }

    pub fn assign(&self, vector: &[f32]) -> Result<u32> {
        nearest_by_l2(&self.centroids, vector)
            .ok_or_else(|| invalid("cannot assign against empty centroid index"))
    }

    /// Approximate nearest-centroid assignment via the HNSW routing layer —
    /// O(log R) instead of `assign`'s O(R) linear scan. The partitioned
    /// billion-scale builder grows the centroid count R with N, so an exact scan
    /// makes the assignment phase O(N*R*dim) ~ quadratic in N; routing through the
    /// HNSW keeps it O(N*log R*dim). Falls back to the exact scan if the HNSW
    /// returns nothing (degenerate/empty index) so assignment never silently drops
    /// a vector.
    pub fn assign_hnsw(&self, vector: &[f32]) -> Result<u32> {
        if self.centroids.is_empty() {
            return Err(invalid("cannot assign against empty centroid index"));
        }
        self.nearest_centroids(vector, 1)
            .first()
            .copied()
            .map_or_else(|| self.assign(vector), Ok)
    }

    /// Approximate raw-L2 nearest-centroid assignment through the metric-aware
    /// centroid graph. This avoids the cosine-only HNSW route while keeping the
    /// assignment phase sublinear in the final centroid count.
    pub fn assign_raw_l2_graph(&self, vector: &[f32]) -> Result<u32> {
        if self.centroids.is_empty() {
            return Err(invalid("cannot assign against empty centroid index"));
        }
        self.nearest_centroids_raw_l2_graph(vector, 1)
            .first()
            .copied()
            .map_or_else(|| self.assign(vector), Ok)
    }

    pub fn nearest_centroids(&self, query: &[f32], n_probe: usize) -> Vec<u32> {
        if self.centroids.is_empty() || n_probe == 0 || query.len() != self.dim as usize {
            return Vec::new();
        }
        let query = SlotVector::Dense {
            dim: self.dim,
            data: query.to_vec(),
        };
        let k = n_probe.min(self.centroids.len());
        self.hnsw
            .search(&query, k, Some(k.max(64)))
            .map(|hits| {
                hits.into_iter()
                    .filter_map(|hit| self.centroid_lookup.get(&hit.cx_id).copied())
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn nearest_centroids_exact_l2(&self, query: &[f32], n_probe: usize) -> Vec<u32> {
        if self.centroids.is_empty() || n_probe == 0 || query.len() != self.dim as usize {
            return Vec::new();
        }
        let k = n_probe.min(self.centroids.len());
        let mut scored: Vec<(u32, f32)> = self
            .centroids
            .iter()
            .enumerate()
            .map(|(idx, centroid)| (idx as u32, l2_sq(centroid, query)))
            .collect();
        scored.sort_by(|a, b| a.1.total_cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
        scored.into_iter().take(k).map(|(idx, _)| idx).collect()
    }

    pub fn nearest_centroids_raw_l2_graph(&self, query: &[f32], n_probe: usize) -> Vec<u32> {
        let ef = n_probe.saturating_mul(4).max(RAW_L2_GRAPH_EF_FLOOR);
        self.raw_l2_graph
            .search(&self.centroids, query, n_probe, ef)
    }

    pub fn save(&self, slot_sparse_dir: impl AsRef<Path>) -> Result<()> {
        self.save_to_path(slot_sparse_dir.as_ref().join("centroids.spn"))
    }

    pub fn save_to_path(&self, path: impl AsRef<Path>) -> Result<()> {
        let path = path.as_ref();
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent).map_err(|e| io("create centroid dir", e))?;
        }
        let tmp = tmp_path(path);
        let file = File::create(&tmp).map_err(|e| io("create centroid tmp", e))?;
        let mut out = BufWriter::new(file);
        write_header(&mut out, self)?;
        for centroid in &self.centroids {
            for value in centroid {
                out.write_all(&value.to_le_bytes())
                    .map_err(|e| io("write centroid f32", e))?;
            }
        }
        for offset in &self.posting_list_offsets {
            out.write_all(&offset.to_le_bytes())
                .map_err(|e| io("write posting offset", e))?;
        }
        for (vector_id, centroid_id) in &self.assignments {
            out.write_all(&vector_id.to_le_bytes())
                .map_err(|e| io("write assignment id", e))?;
            out.write_all(&centroid_id.to_le_bytes())
                .map_err(|e| io("write assignment centroid", e))?;
        }
        let file = out
            .into_inner()
            .map_err(|e| io("flush centroid tmp", e.into_error()))?;
        file.sync_all().map_err(|e| io("fsync centroid tmp", e))?;
        drop(file);
        fs::rename(&tmp, path).map_err(|e| io("publish centroids", e))
    }

    pub fn open(slot_sparse_dir: impl AsRef<Path>) -> Result<Self> {
        Self::open_from_path(slot_sparse_dir.as_ref().join("centroids.spn"))
    }

    pub fn open_from_path(path: impl AsRef<Path>) -> Result<Self> {
        let bytes = fs::read(path.as_ref()).map_err(|e| io("read centroids", e))?;
        decode_centroids(&bytes)
    }
}

pub fn build_centroids(
    vectors: &[(u32, Vec<f32>)],
    n_clusters: usize,
    seed: u64,
) -> SpannCentroidIndex {
    try_build_centroids(vectors, n_clusters, seed).expect("valid SPANN centroid input")
}

pub fn try_build_centroids(
    vectors: &[(u32, Vec<f32>)],
    n_clusters: usize,
    seed: u64,
) -> Result<SpannCentroidIndex> {
    if vectors.is_empty() {
        return Ok(SpannCentroidIndex::empty(0));
    }
    let dim = vectors[0].1.len();
    validate_rows(vectors, dim)?;
    let k = effective_cluster_count(vectors.len(), n_clusters);
    let mut centroids = kmeans_pp(vectors, k, seed);
    for _ in 0..KMEANS_ITERS {
        // Assignment step is O(N*k*dim) and the hot loop — compute the nearest
        // centroid for every vector in parallel, then accumulate sums serially in
        // index order (so the float reduction stays deterministic / unchanged).
        let assignments: Vec<u32> = vectors
            .par_iter()
            .map(|(_, vector)| nearest_by_l2(&centroids, vector).expect("k > 0"))
            .collect();
        let mut sums = vec![vec![0.0_f32; dim]; k];
        let mut counts = vec![0_usize; k];
        for ((_, vector), &cid) in vectors.iter().zip(&assignments) {
            counts[cid as usize] += 1;
            for (sum, value) in sums[cid as usize].iter_mut().zip(vector) {
                *sum += *value;
            }
        }
        for cid in 0..k {
            if counts[cid] == 0 {
                centroids[cid] = farthest_vector(vectors, &centroids).clone();
            } else {
                let inv = 1.0 / counts[cid] as f32;
                for value in &mut sums[cid] {
                    *value *= inv;
                }
                centroids[cid] = sums[cid].clone();
            }
        }
    }
    let assignments = vectors
        .par_iter()
        .map(|(id, vector)| (*id, nearest_by_l2(&centroids, vector).expect("k > 0")))
        .collect();
    SpannCentroidIndex::from_parts(dim as u32, centroids, (0..k as u64).collect(), assignments)
}

pub fn default_cluster_count(vector_count: usize) -> usize {
    ((vector_count as f64).sqrt() as usize).max(1)
}

fn effective_cluster_count(vector_count: usize, requested: usize) -> usize {
    let wanted = if requested == 0 {
        default_cluster_count(vector_count)
    } else {
        requested
    };
    wanted.min(vector_count)
}

fn kmeans_pp(vectors: &[(u32, Vec<f32>)], k: usize, seed: u64) -> Vec<Vec<f32>> {
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    let first = vectors[rng.random_range(0..vectors.len())].1.clone();
    // Canonical kmeans++ with a cached D^2 array: `min_dist[i]` is the squared
    // distance from vector i to its NEAREST chosen centroid so far. Adding a
    // centroid updates the cache in O(N*dim) instead of re-scanning the whole
    // centroid set (the old code was O(N*k^2*dim) — the billion-scale build wall).
    // The cache equals `nearest_distance_sq(&centroids, v)` at every step, so the
    // RNG draws and chosen centroids are bit-identical to the naive version.
    let mut min_dist: Vec<f32> = vectors
        .par_iter()
        .map(|(_, vector)| l2_sq(&first, vector))
        .collect();
    let mut centroids = vec![first];
    while centroids.len() < k {
        let total: f32 = min_dist.iter().sum();
        if total <= f32::EPSILON {
            // Degenerate: every remaining point coincides with a centroid. Mirror
            // the naive fallback exactly, then refresh the cache against the new
            // centroid set (rare path — correctness over speed).
            let before = centroids.len();
            for (_, vector) in vectors {
                if !centroids.contains(vector) {
                    centroids.push(vector.clone());
                    break;
                }
            }
            if centroids.len() < k {
                centroids.push(vectors[centroids.len() % vectors.len()].1.clone());
            }
            if centroids.len() != before {
                let cset = &centroids;
                min_dist = vectors
                    .par_iter()
                    .map(|(_, vector)| nearest_distance_sq(cset, vector))
                    .collect();
            }
            continue;
        }
        let mut cut = rng.random_range(0.0..total);
        let mut chosen = vectors.len() - 1;
        for (idx, distance) in min_dist.iter().enumerate() {
            cut -= *distance;
            if cut <= 0.0 {
                chosen = idx;
                break;
            }
        }
        let new_centroid = vectors[chosen].1.clone();
        // Fold the new centroid into the cached nearest-distance array in parallel.
        min_dist
            .par_iter_mut()
            .zip(vectors.par_iter())
            .for_each(|(md, (_, vector))| {
                let d = l2_sq(&new_centroid, vector);
                if d < *md {
                    *md = d;
                }
            });
        centroids.push(new_centroid);
    }
    centroids
}

fn farthest_vector<'a>(vectors: &'a [(u32, Vec<f32>)], centroids: &[Vec<f32>]) -> &'a Vec<f32> {
    vectors
        .iter()
        .map(|(_, vector)| (vector, nearest_distance_sq(centroids, vector)))
        .max_by(|(_, a_distance), (_, b_distance)| a_distance.total_cmp(b_distance))
        .map(|(vector, _)| vector)
        .expect("non-empty vectors")
}

fn nearest_by_l2(centroids: &[Vec<f32>], vector: &[f32]) -> Option<u32> {
    centroids
        .iter()
        .enumerate()
        .map(|(idx, centroid)| (idx, l2_sq(centroid, vector)))
        .min_by(|(a_idx, a_distance), (b_idx, b_distance)| {
            a_distance
                .total_cmp(b_distance)
                .then_with(|| a_idx.cmp(b_idx))
        })
        .map(|(idx, _distance)| idx as u32)
}

fn nearest_distance_sq(centroids: &[Vec<f32>], vector: &[f32]) -> f32 {
    centroids
        .iter()
        .map(|centroid| l2_sq(centroid, vector))
        .min_by(f32::total_cmp)
        .unwrap_or(0.0)
}

fn build_hnsw(dim: u32, centroids: &[Vec<f32>]) -> Result<(HnswIndex, BTreeMap<CxId, u32>)> {
    let mut hnsw = HnswIndex::new(CENTROID_SLOT, dim, 0x5A17_570A);
    let mut lookup = BTreeMap::new();
    for (idx, vector) in centroids.iter().enumerate() {
        let id = centroid_cx_id(idx as u32);
        hnsw.insert(
            id,
            SlotVector::Dense {
                dim,
                data: vector.clone(),
            },
            idx as u64,
        )?;
        lookup.insert(id, idx as u32);
    }
    Ok((hnsw, lookup))
}

fn centroid_cx_id(id: u32) -> CxId {
    let mut bytes = [0_u8; 16];
    bytes[0..8].copy_from_slice(b"CLXSPANN");
    bytes[12..16].copy_from_slice(&id.to_be_bytes());
    CxId::from_bytes(bytes)
}

fn validate_rows(vectors: &[(u32, Vec<f32>)], dim: usize) -> Result<()> {
    for (id, vector) in vectors {
        if vector.len() != dim {
            return Err(sextant_error(
                CALYX_INDEX_DIM_MISMATCH,
                format!("vector {id} dim {} expected {dim}", vector.len()),
            ));
        }
        if vector.iter().any(|value| !value.is_finite()) {
            return Err(invalid(format!("vector {id} has non-finite component")));
        }
    }
    Ok(())
}

fn validate_centroids(dim: u32, centroids: &[Vec<f32>]) -> Result<()> {
    for (idx, centroid) in centroids.iter().enumerate() {
        if centroid.len() != dim as usize {
            return Err(sextant_error(
                CALYX_INDEX_DIM_MISMATCH,
                format!("centroid {idx} dim {} expected {dim}", centroid.len()),
            ));
        }
        if centroid.iter().any(|value| !value.is_finite()) {
            return Err(invalid(format!("centroid {idx} has non-finite component")));
        }
    }
    Ok(())
}

fn tmp_path(path: &Path) -> PathBuf {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    PathBuf::from(tmp)
}

fn invalid(detail: impl std::fmt::Display) -> calyx_core::CalyxError {
    sextant_error(
        CALYX_INDEX_INVALID_PARAMS,
        format!("spann centroids: {detail}"),
    )
}

fn corrupt(detail: impl std::fmt::Display) -> calyx_core::CalyxError {
    sextant_error(
        CALYX_INDEX_CORRUPT,
        format!("spann centroids corrupt: {detail}"),
    )
}

fn io(stage: &str, error: std::io::Error) -> calyx_core::CalyxError {
    sextant_error(CALYX_INDEX_IO, format!("spann centroids {stage}: {error}"))
}
