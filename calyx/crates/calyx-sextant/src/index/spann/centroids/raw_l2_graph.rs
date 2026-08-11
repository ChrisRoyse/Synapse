use std::cmp::Ordering;
use std::collections::BinaryHeap;

use rayon::prelude::*;

use crate::index::distance::l2_sq;

const RAW_L2_GRAPH_DEGREE: usize = 32;
const RAW_L2_GRAPH_MAX_DEGREE: usize = RAW_L2_GRAPH_DEGREE * 2;

#[derive(Clone, Debug, Default)]
pub(super) struct RawL2CentroidGraph {
    neighbors: Vec<Vec<u32>>,
}

impl RawL2CentroidGraph {
    pub fn build(centroids: &[Vec<f32>]) -> Self {
        if centroids.is_empty() {
            return Self::default();
        }
        let scratch_capacity = centroids.len().saturating_sub(1);
        let directed: Vec<Vec<u32>> = (0..centroids.len())
            .into_par_iter()
            .map_init(
                || Vec::with_capacity(scratch_capacity),
                |scored, idx| nearest_neighbors(idx, centroids, RAW_L2_GRAPH_DEGREE, scored),
            )
            .collect();
        let mut neighbors = directed.clone();
        for (idx, row) in directed.iter().enumerate() {
            for &neighbor in row {
                neighbors[neighbor as usize].push(idx as u32);
            }
        }
        let neighbors = neighbors
            .into_par_iter()
            .enumerate()
            .map(|(idx, mut row)| {
                row.sort_unstable();
                row.dedup();
                prune_neighbors(idx, centroids, row)
            })
            .collect();
        Self { neighbors }
    }

    pub fn search(
        &self,
        centroids: &[Vec<f32>],
        query: &[f32],
        n_probe: usize,
        ef: usize,
    ) -> Vec<u32> {
        if centroids.is_empty() || n_probe == 0 || query.len() != centroids[0].len() {
            return Vec::new();
        }
        let k = n_probe.min(centroids.len());
        let ef = ef.max(k).min(centroids.len());
        if ef == centroids.len() || self.neighbors.len() != centroids.len() {
            return exact_l2(centroids, query, k);
        }
        let mut seen = vec![false; centroids.len()];
        let mut heap = BinaryHeap::new();
        for entry in entry_points(centroids.len()) {
            push_candidate(entry, centroids, query, &mut seen, &mut heap);
        }
        let mut scored = Vec::with_capacity(ef);
        while let Some(candidate) = heap.pop() {
            scored.push((candidate.idx, candidate.distance));
            if scored.len() >= ef {
                break;
            }
            for &neighbor in &self.neighbors[candidate.idx as usize] {
                push_candidate(neighbor, centroids, query, &mut seen, &mut heap);
            }
        }
        if scored.len() < k {
            return exact_l2(centroids, query, k);
        }
        sort_l2_scored(&mut scored);
        scored.truncate(k);
        scored.into_iter().map(|(idx, _)| idx).collect()
    }
}

fn nearest_neighbors(
    idx: usize,
    centroids: &[Vec<f32>],
    limit: usize,
    scored: &mut Vec<(u32, f32)>,
) -> Vec<u32> {
    // The old map/collect allocated an R-wide buffer for every centroid. Rayon
    // now retains one scratch allocation per job and refills it, while exact
    // distance and total-order selection remain unchanged.
    scored.clear();
    scored.extend(
        centroids
            .iter()
            .enumerate()
            .filter(|(other, _)| *other != idx)
            .map(|(other, centroid)| (other as u32, l2_sq(&centroids[idx], centroid))),
    );
    take_nearest(scored, limit)
}

fn prune_neighbors(idx: usize, centroids: &[Vec<f32>], candidates: Vec<u32>) -> Vec<u32> {
    let mut scored: Vec<(u32, f32)> = candidates
        .into_iter()
        .filter(|&other| other as usize != idx)
        .map(|other| (other, l2_sq(&centroids[idx], &centroids[other as usize])))
        .collect();
    take_nearest(&mut scored, RAW_L2_GRAPH_MAX_DEGREE)
}

fn take_nearest(scored: &mut Vec<(u32, f32)>, limit: usize) -> Vec<u32> {
    if scored.is_empty() {
        return Vec::new();
    }
    let limit = limit.min(scored.len());
    if limit < scored.len() {
        scored.select_nth_unstable_by(limit, cmp_l2_scored);
        scored.truncate(limit);
    }
    sort_l2_scored(scored);
    scored.iter().map(|(idx, _)| *idx).collect()
}

fn exact_l2(centroids: &[Vec<f32>], query: &[f32], k: usize) -> Vec<u32> {
    let mut scored: Vec<(u32, f32)> = centroids
        .iter()
        .enumerate()
        .map(|(idx, centroid)| (idx as u32, l2_sq(centroid, query)))
        .collect();
    take_nearest(&mut scored, k)
}

fn entry_points(len: usize) -> Vec<u32> {
    let mut entries = vec![0, len / 2, len.saturating_sub(1), len / 4, (len * 3) / 4];
    entries.sort_unstable();
    entries.dedup();
    entries.into_iter().map(|idx| idx as u32).collect()
}

fn push_candidate(
    idx: u32,
    centroids: &[Vec<f32>],
    query: &[f32],
    seen: &mut [bool],
    heap: &mut BinaryHeap<HeapCandidate>,
) {
    let idx_usize = idx as usize;
    if idx_usize >= centroids.len() || seen[idx_usize] {
        return;
    }
    seen[idx_usize] = true;
    heap.push(HeapCandidate {
        idx,
        distance: l2_sq(&centroids[idx_usize], query),
    });
}

fn sort_l2_scored(scored: &mut [(u32, f32)]) {
    scored.sort_by(cmp_l2_scored);
}

fn cmp_l2_scored(left: &(u32, f32), right: &(u32, f32)) -> Ordering {
    left.1
        .total_cmp(&right.1)
        .then_with(|| left.0.cmp(&right.0))
}

#[derive(Clone, Copy, Debug)]
struct HeapCandidate {
    idx: u32,
    distance: f32,
}

impl PartialEq for HeapCandidate {
    fn eq(&self, other: &Self) -> bool {
        self.idx == other.idx && self.distance.to_bits() == other.distance.to_bits()
    }
}

impl Eq for HeapCandidate {}

impl PartialOrd for HeapCandidate {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for HeapCandidate {
    fn cmp(&self, other: &Self) -> Ordering {
        other
            .distance
            .total_cmp(&self.distance)
            .then_with(|| other.idx.cmp(&self.idx))
    }
}
