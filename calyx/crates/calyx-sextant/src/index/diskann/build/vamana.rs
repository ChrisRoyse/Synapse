use std::cell::RefCell;
use std::cmp::{Ordering, Reverse};
use std::collections::{BinaryHeap, HashSet};

use calyx_core::Result;
use rand::seq::SliceRandom;
use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha8Rng;
use rayon::prelude::*;

use super::metric::{BuildSpace, DiskAnnBuildMetric, build_space, dist};
use super::{DiskAnnBuildParams, DiskAnnBuildProgress};

/// Deterministic build seed (Vamana insert order + random init edges).
const BUILD_SEED: u64 = 42;
/// First synchronization round size. Batches grow geometrically from here
/// (ParlayANN prefix-doubling): early points refine the graph at near-
/// sequential quality, later points parallelize over the larger snapshot.
const BUILD_BATCH_MIN: usize = 256;
/// Batches never exceed `n / BUILD_BATCH_DIVISOR` so that no single
/// synchronization round connects more than a small fraction of the graph
/// against one stale snapshot -- keeping graph quality scale-independent.
const BUILD_BATCH_DIVISOR: usize = 32;
const BUILD_PROGRESS_ROWS: usize = 4096;

/// Fixed-stride adjacency: `n * m_max` neighbour slots in one block plus a
/// per-node degree.
///
/// Every row this build ever stores is at most `m_max` wide — `initial_neighbors`
/// caps at `min(m_max, n-1)`, `robust_prune` returns at most `r = m_max`, and the
/// back-edge merge only skips the prune when it is already within `m_max`. So a
/// `Vec<Vec<u32>>` bought nothing but an allocation, a free, and a pointer chase
/// per row. Two batch phases used to pay for that in the worst possible place:
/// the forward and back-edge writebacks are *sequential* (they scatter into the
/// shared graph), so on a 32-core host they were pure Amdahl serial time. Here
/// a writeback is a `copy_from_slice`.
struct Adjacency {
    flat: Vec<u32>,
    degree: Vec<u32>,
    stride: usize,
}

impl Adjacency {
    fn new(node_count: usize, stride: usize) -> Self {
        Self {
            flat: vec![0; node_count * stride],
            degree: vec![0; node_count],
            stride,
        }
    }

    fn neighbors(&self, node: usize) -> &[u32] {
        let start = node * self.stride;
        &self.flat[start..start + self.degree[node] as usize]
    }

    fn set(&mut self, node: usize, neighbors: &[u32]) {
        let start = node * self.stride;
        self.flat[start..start + neighbors.len()].copy_from_slice(neighbors);
        self.degree[node] = neighbors.len() as u32;
    }

    /// Row-per-node view for the graph writer, materialized once at the end.
    fn into_rows(self) -> Vec<Vec<u32>> {
        self.flat
            .chunks_exact(self.stride)
            .zip(&self.degree)
            .map(|(row, degree)| row[..*degree as usize].to_vec())
            .collect()
    }
}

/// Two-pass Vamana over an in-memory adjacency list, batched + parallel.
pub(super) fn vamana<F>(
    vectors: &[(u32, Vec<f32>)],
    params: &DiskAnnBuildParams,
    metric: DiskAnnBuildMetric,
    progress: &mut F,
) -> Result<(u32, Vec<Vec<u32>>)>
where
    F: FnMut(DiskAnnBuildProgress) -> Result<()>,
{
    let n = vectors.len();
    if n == 1 {
        return Ok((0, vec![Vec::new()]));
    }
    progress(DiskAnnBuildProgress::new("diskann_space_start", 0))?;
    let space = build_space(vectors, params.dim, metric);
    let entry = medoid(&space, metric);
    progress(DiskAnnBuildProgress::new("diskann_space_ok", n))?;
    let mut rng = ChaCha8Rng::seed_from_u64(BUILD_SEED);
    let init_degree = params.m_max.min(n - 1);
    let mut adjacency = Adjacency::new(n, params.m_max);
    progress(DiskAnnBuildProgress::new("diskann_init_start", 0))?;
    let mut initial = Vec::with_capacity(init_degree);
    for i in 0..n as u32 {
        initial_neighbors(n as u32, i, init_degree, entry, &mut rng, &mut initial);
        adjacency.set(i as usize, &initial);
        let initialized = i as usize + 1;
        if initialized == n || initialized.is_multiple_of(BUILD_PROGRESS_ROWS) {
            progress(DiskAnnBuildProgress::new("diskann_init_page", initialized))?;
        }
    }
    progress(DiskAnnBuildProgress::new("diskann_init_ok", n))?;
    let ef = params.ef_construction.max(params.m_max);
    let mut order: Vec<u32> = (0..n as u32).collect();
    let batch_cap = (n / BUILD_BATCH_DIVISOR).max(BUILD_BATCH_MIN);
    let mut edges: Vec<(u32, u32)> = Vec::new();
    for (pass_idx, alpha) in [1.0_f32, params.alpha].into_iter().enumerate() {
        let (pass_start, pass_page, pass_ok) = match pass_idx {
            0 => (
                "diskann_vamana_pass1_start",
                "diskann_vamana_pass1_batch_ok",
                "diskann_vamana_pass1_ok",
            ),
            _ => (
                "diskann_vamana_pass2_start",
                "diskann_vamana_pass2_batch_ok",
                "diskann_vamana_pass2_ok",
            ),
        };
        progress(DiskAnnBuildProgress::new(pass_start, 0))?;
        order.shuffle(&mut rng);
        let mut start = 0;
        let mut batch_size = BUILD_BATCH_MIN;
        while start < order.len() {
            let end = (start + batch_size).min(order.len());
            let batch = &order[start..end];
            start = end;
            batch_size = (batch_size * 2).min(batch_cap);
            // Parallel, read-only against the frozen `adjacency` snapshot.
            let pruned: Vec<(u32, Vec<u32>)> = batch
                .par_iter()
                .map(|&i| {
                    BUILD_SCRATCH.with_borrow_mut(|scratch| {
                        let mut candidates =
                            greedy_search(scratch, &space, &adjacency, entry, i, ef, metric);
                        candidates.extend_from_slice(adjacency.neighbors(i as usize));
                        let neighbors = robust_prune(
                            scratch,
                            &space,
                            i,
                            candidates,
                            alpha,
                            params.m_max,
                            metric,
                        );
                        (i, neighbors)
                    })
                })
                .collect();
            // Forward edges: sequential, but now a memcpy per row.
            for (i, neighbors) in &pruned {
                adjacency.set(*i as usize, neighbors);
            }
            // Back-edges grouped by target. This used to be a sequential
            // `BTreeMap<u32, Vec<u32>>` fill — one tree descent and one `Vec`
            // allocation per edge, ~17% of the whole build on one core. A
            // *stable* parallel sort by target reproduces it exactly: stability
            // preserves the batch-order add-lists the map's `push` produced, and
            // sorted order reproduces the map's ascending-key iteration, so the
            // groups handed to the re-prune are identical run for run.
            edges.clear();
            edges.reserve(pruned.iter().map(|(_, ns)| ns.len()).sum());
            for (i, neighbors) in &pruned {
                for &j in neighbors {
                    edges.push((j, *i));
                }
            }
            edges.par_sort_by_key(|&(j, _)| j);
            let groups: Vec<&[(u32, u32)]> = edges.chunk_by(|a, b| a.0 == b.0).collect();
            // Each affected node is re-pruned ONCE for the whole batch, and the
            // re-prunes run in parallel -- this is the build's hot path, so it
            // must not serialize.
            let updates: Vec<(u32, Vec<u32>)> = groups
                .par_iter()
                .map(|group| {
                    let j = group[0].0;
                    let mut merged = adjacency.neighbors(j as usize).to_vec();
                    for &(_, i) in *group {
                        if !merged.contains(&i) {
                            merged.push(i);
                        }
                    }
                    let neighbors = if merged.len() > params.m_max {
                        BUILD_SCRATCH.with_borrow_mut(|scratch| {
                            robust_prune(scratch, &space, j, merged, alpha, params.m_max, metric)
                        })
                    } else {
                        merged
                    };
                    (j, neighbors)
                })
                .collect();
            for (j, neighbors) in updates {
                adjacency.set(j as usize, &neighbors);
            }
            progress(DiskAnnBuildProgress::new(pass_page, end))?;
        }
        progress(DiskAnnBuildProgress::new(pass_ok, n))?;
    }
    Ok((entry, adjacency.into_rows()))
}

fn initial_neighbors(
    node_count: u32,
    node: u32,
    degree: usize,
    entry: u32,
    rng: &mut ChaCha8Rng,
    out: &mut Vec<u32>,
) {
    out.clear();
    let mut seen = HashSet::with_capacity(degree);
    if node != entry && degree > 0 {
        out.push(entry);
        seen.insert(entry);
    }
    while out.len() < degree {
        let candidate = rng.random_range(0..node_count);
        if candidate != node && seen.insert(candidate) {
            out.push(candidate);
        }
    }
}

/// Point closest to the active build-space centroid — the DiskANN entry.
///
/// The centroid accumulates in row order and the argmin scan keeps the strict
/// `<` first-wins tie-break, so this picks the same point the row-of-`Vec`
/// version did. It is `O(n)` distances against one fixed query — 0.13% of build
/// wall-clock at production shape (41 ms of 15.9 s, measured), which is why it
/// stays scalar and sequential.
pub(in crate::index::diskann) fn medoid(space: &BuildSpace, metric: DiskAnnBuildMetric) -> u32 {
    let dim = space.dim();
    let mut centroid = vec![0.0_f32; dim];
    for id in 0..space.len() {
        for (c, x) in centroid.iter_mut().zip(space.row(id)) {
            *c += x;
        }
    }
    let inv = 1.0 / space.len() as f32;
    for c in &mut centroid {
        *c *= inv;
    }
    let mut best = (0_u32, f32::INFINITY);
    for id in 0..space.len() {
        let d = dist(&centroid, space.row(id), metric);
        if d < best.1 {
            best = (id as u32, d);
        }
    }
    best.0
}

/// A `(distance, id)` candidate ordered exactly as the build's comparator was:
/// `total_cmp` on the distance, then the id. That pair is a *total* order over
/// distinct ids, which is what licenses the heap rewrite below — see
/// `greedy_search`.
#[derive(Clone, Copy, PartialEq)]
struct Candidate {
    distance: f32,
    id: u32,
}

impl Eq for Candidate {}

impl Ord for Candidate {
    fn cmp(&self, other: &Self) -> Ordering {
        self.distance
            .total_cmp(&other.distance)
            .then_with(|| self.id.cmp(&other.id))
    }
}

impl PartialOrd for Candidate {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

const SEEN: u32 = 1;
const EXPANDED: u32 = 1 << 1;
const EVICTED: u32 = 1 << 2;
const DEDUPED: u32 = 1 << 3;
/// Width of the flag field in a mark word; the rest of the word is the epoch.
const FLAG_BITS: u32 = 4;
const MAX_EPOCH: u32 = u32::MAX >> FLAG_BITS;

/// Per-thread build scratch: epoch-stamped membership flags plus the two heaps
/// the beam search needs, all reused across calls.
///
/// The previous implementation allocated two `HashSet<u32>`s per `greedy_search`
/// call and a third per `robust_prune` call — on the order of a million sets
/// over a production build — and paid SipHash on every neighbour touch. Epoch
/// stamping replaces both: a node's word carries the epoch it was last touched
/// in, so anything stamped with an older epoch reads as "unset" and a call
/// resets its state in `O(1)` by bumping `epoch` instead of clearing `O(n)`.
///
/// This mirrors `index/diskann/search/scratch.rs`, which the *query* path has
/// used since PH68; the build path never got the same treatment.
#[derive(Default)]
struct BuildScratch {
    epoch: u32,
    /// One word per node: `epoch << FLAG_BITS | flags`. Epoch and flags share a
    /// word deliberately — a membership test in the beam loop is itself a random
    /// access, so splitting them across two arrays doubled the cache lines the
    /// hottest loop touches for no benefit. Four flag bits leave 2^28 epochs,
    /// far beyond the ~10^5 calls a thread makes in a build, and the wrap is
    /// handled anyway.
    marks: Vec<u32>,
    /// Max-heap over `(distance, id)`: the bounded beam. Its root is the worst
    /// member, which is the one `truncate(ef)` used to drop.
    beam: BinaryHeap<Candidate>,
    /// Min-heap over the same order: candidates not yet expanded.
    frontier: BinaryHeap<Reverse<Candidate>>,
    prune_pool: Vec<(u32, f32)>,
}

thread_local! {
    static BUILD_SCRATCH: RefCell<BuildScratch> = RefCell::new(BuildScratch::default());
}

impl BuildScratch {
    fn begin(&mut self, node_count: usize) {
        if self.marks.len() != node_count {
            self.marks.clear();
            self.marks.resize(node_count, 0);
            self.epoch = 0;
        }
        if self.epoch >= MAX_EPOCH {
            self.marks.fill(0);
            self.epoch = 1;
        } else {
            self.epoch += 1;
        }
    }

    fn has_any(&self, id: u32, mask: u32) -> bool {
        let word = self.marks[id as usize];
        word >> FLAG_BITS == self.epoch && word & mask != 0
    }

    fn mark(&mut self, id: u32, bits: u32) {
        let slot = &mut self.marks[id as usize];
        if *slot >> FLAG_BITS == self.epoch {
            *slot |= bits;
        } else {
            *slot = (self.epoch << FLAG_BITS) | bits;
        }
    }
}

/// Greedy beam search over the in-memory adjacency from `entry` toward
/// `query` (a node id); returns every expanded node (the prune candidate set).
///
/// # Why this is the same search the sort-based version ran
///
/// The original kept the beam as a `Vec` that it re-sorted by
/// `total_cmp().then(id)` and truncated to `ef` after *every* expansion, then
/// scanned that `Vec` linearly for the first unexpanded member — `O(ef log ef)`
/// ordering work and `O(ef)` hashed membership probes per expansion, on top of
/// the distances.
///
/// Because `(distance, id)` under that comparator is a **total order on
/// distinct ids**, the state the loop actually depends on is only:
///
/// 1. *which* ids are still in the beam — i.e. the `ef` smallest of everything
///    inserted so far. Incremental "sort, keep smallest `ef`" and global "keep
///    smallest `ef`" coincide: an element outside the top `ef` of a prefix is
///    dominated by `ef` elements that are all still present later, so it can
///    never re-enter. Nothing is ever removed except by truncation, and `SEEN`
///    forbids re-insertion, so the two agree exactly.
/// 2. the *minimum* unexpanded member of that set.
///
/// A max-heap bounded to `ef` maintains (1) — popping its root drops precisely
/// the element `truncate` dropped — and a min-heap over the same order yields
/// (2), skipping ids already expanded or already evicted. Ties are impossible
/// (ids are distinct), so no comparator ambiguity can leak in. The expansion
/// sequence, and therefore `visited`, is bit-identical; only the bookkeeping
/// cost changes, from `O(ef)` per expansion to `O(log ef)` per candidate.
fn greedy_search(
    scratch: &mut BuildScratch,
    space: &BuildSpace,
    adjacency: &Adjacency,
    entry: u32,
    query: u32,
    ef: usize,
    metric: DiskAnnBuildMetric,
) -> Vec<u32> {
    scratch.begin(space.len());
    scratch.beam.clear();
    scratch.frontier.clear();
    let q = space.row(query as usize);
    let seed = Candidate {
        distance: dist(q, space.row(entry as usize), metric),
        id: entry,
    };
    scratch.mark(entry, SEEN);
    scratch.beam.push(seed);
    scratch.frontier.push(Reverse(seed));
    let mut visited: Vec<u32> = Vec::new();
    while let Some(next) = pop_next(scratch) {
        scratch.mark(next, EXPANDED);
        visited.push(next);
        let neighbors = adjacency.neighbors(next as usize);
        // Every one of these rows is a random read into a 144 MiB space and a
        // near-certain cache+TLB miss, and the distance that consumes it is
        // ~10 ns of AVX2. Issued one at a time the loop is a chain of ~450 ns
        // stalls; issued together they overlap. The whole list is known here,
        // so there is no reason to discover it one element at a time.
        for &neighbor in neighbors {
            space.prefetch(neighbor as usize);
        }
        for &neighbor in neighbors {
            if scratch.has_any(neighbor, SEEN) {
                continue;
            }
            scratch.mark(neighbor, SEEN);
            let candidate = Candidate {
                distance: dist(q, space.row(neighbor as usize), metric),
                id: neighbor,
            };
            // A candidate worse than a full beam's worst member is dead on
            // arrival: truncation only ever gets more aggressive as the round
            // adds more candidates, so nothing later in this round can rescue
            // it. Recording the eviction directly, instead of pushing it onto
            // both heaps only to pop it again, keeps the frontier at beam scale
            // rather than at total-candidates scale.
            if scratch.beam.len() >= ef
                && scratch.beam.peek().is_some_and(|worst| candidate > *worst)
            {
                scratch.mark(neighbor, EVICTED);
                continue;
            }
            scratch.beam.push(candidate);
            scratch.frontier.push(Reverse(candidate));
            // Same drop set as `sort_by(...); truncate(ef)`: shed the maximum
            // whenever the beam overflows. Keeping only the `ef` smallest is
            // order-independent, so doing it per insertion rather than once per
            // round reaches the same beam.
            while scratch.beam.len() > ef {
                let Some(worst) = scratch.beam.pop() else {
                    break;
                };
                scratch.mark(worst.id, EVICTED);
            }
        }
    }
    visited
}

/// Smallest beam member that is neither expanded nor evicted, i.e. exactly what
/// `pool.iter().find(|c| !expanded.contains(c))` returned from the sorted pool.
fn pop_next(scratch: &mut BuildScratch) -> Option<u32> {
    while let Some(Reverse(candidate)) = scratch.frontier.pop() {
        if scratch.has_any(candidate.id, EXPANDED | EVICTED) {
            continue;
        }
        return Some(candidate.id);
    }
    None
}

/// RobustPrune(p, candidates, alpha, r): keep the closest candidate, drop any
/// other whose distance to it (scaled by alpha) undercuts its distance to p.
///
/// Candidate de-duplication moved off `HashSet<u32>` onto the epoch-stamped
/// flags. That is safe where it matters: the deduplicated set is identical, and
/// the very next statement sorts it under a total order on distinct ids, so the
/// surviving sequence cannot depend on how duplicates were filtered (nor on the
/// sort being stable).
///
/// The `retain` block stays scalar on purpose. It is a genuine `1 x |pool|`
/// batch shape, but `|pool|` is a few hundred rows of `dim` floats and the loop
/// runs once per prune round — dispatching that to a GPU would be tens of
/// millions of sub-microsecond kernel launches. See #2103 for the measurement.
fn robust_prune(
    scratch: &mut BuildScratch,
    space: &BuildSpace,
    p: u32,
    candidates: Vec<u32>,
    alpha: f32,
    r: usize,
    metric: DiskAnnBuildMetric,
) -> Vec<u32> {
    scratch.begin(space.len());
    let q = space.row(p as usize);
    let mut pool = std::mem::take(&mut scratch.prune_pool);
    pool.clear();
    pool.reserve(candidates.len());
    for c in candidates {
        if c == p || scratch.has_any(c, DEDUPED) {
            continue;
        }
        scratch.mark(c, DEDUPED);
        pool.push((c, dist(q, space.row(c as usize), metric)));
    }
    pool.sort_unstable_by(|a, b| a.1.total_cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
    let mut result: Vec<u32> = Vec::with_capacity(r);
    while let Some((star, _)) = pool.first().copied() {
        result.push(star);
        if result.len() >= r {
            break;
        }
        let star_vec = space.row(star as usize);
        pool.retain(|&(c, d_pc)| {
            c != star && alpha * dist(star_vec, space.row(c as usize), metric) > d_pc
        });
    }
    scratch.prune_pool = pool;
    result
}
