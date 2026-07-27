//! Plain graph key-encoding layer for 0-lens collections.

mod assoc_graph;
mod csr_store;
mod key;
mod lifecycle;
mod physical;
mod types;

use std::collections::{BTreeMap, BTreeSet};

use calyx_core::{Clock, CxId, Result, Seq};
use calyx_paths::AssocGraph;

use crate::cf::{ColumnFamily, KeyRange};
use crate::vault::AsterVault;
use assoc_graph::{assoc_graph_from_csr, flatten_csr_edges};
use key::{
    GraphKeyspace, MAX_TRAVERSE_COST, MAX_TRAVERSE_HOPS, all_graph_rows_range, collection_from_key,
    graph_corrupt, graph_limit, graph_missing, path_error, validate_edge_type, validate_value,
};

pub use lifecycle::{
    GraphCollectionGenerationReadback, GraphCollectionGenerationState,
    GraphCollectionGenerationStatus, GraphCollectionLifecycle, PhysicalGraphCollectionLifecycle,
};
pub use physical::PhysicalPlainGraph;
pub use types::{
    CsrCommit, GraphEdgeCommit, PlainGraphCsr, PlainGraphCsrEdge, PlainGraphDirection,
    PlainGraphEdge, PlainGraphEdgeWeightStats, TraverseOptions, plain_graph_edge_raw_weight,
    plain_graph_normalized_edge_weight,
};
use types::{EdgeWeightPolicy, EdgeWeightSource, plain_graph_edge_raw_weight_with_policy};

#[derive(Clone, Debug)]
struct WeightedEdgeDraft {
    src: CxId,
    src_index: usize,
    dst: CxId,
    edge_type: String,
    raw_weight: f32,
}

/// Discovers every plain-graph collection from physical Graph CF keys without
/// materializing each collection. The cursor jumps to the end of the current
/// length-delimited collection prefix, so discovery cost follows collection
/// count instead of graph size.
pub(crate) fn graph_storage_collections<C: Clock>(
    vault: &AsterVault<C>,
    snapshot: Seq,
) -> Result<Vec<String>> {
    let all_rows = all_graph_rows_range();
    let mut after_key = None::<Vec<u8>>;
    let mut collections = BTreeSet::new();
    loop {
        let page = vault.scan_cf_range_page_at(
            snapshot,
            ColumnFamily::Graph,
            &all_rows,
            after_key.as_deref(),
            1,
        )?;
        let Some((key, _)) = page.first() else {
            break;
        };
        let collection = collection_from_key(key)?;
        let keyspace = GraphKeyspace::new(&collection)?;
        after_key = keyspace.collection_range().end;
        if after_key.is_none() {
            return Err(graph_corrupt(format!(
                "graph collection {collection} has no finite key range"
            )));
        }
        collections.insert(collection);
    }
    Ok(collections.into_iter().collect())
}

pub struct PlainGraph<'a, C: Clock> {
    vault: &'a AsterVault<C>,
    keys: GraphKeyspace,
}

impl<'a, C: Clock> PlainGraph<'a, C> {
    pub fn new(vault: &'a AsterVault<C>, collection: &str) -> Result<Self> {
        Ok(Self {
            vault,
            keys: GraphKeyspace::new(collection)?,
        })
    }

    pub fn put_node(&self, node: CxId, props: &[u8]) -> Result<Seq> {
        validate_value("node props", props)?;
        self.vault
            .write_cf(ColumnFamily::Graph, self.node_key(node), props.to_vec())
    }

    pub fn put_edge(
        &self,
        src: CxId,
        edge_type: &str,
        dst: CxId,
        value: &[u8],
    ) -> Result<GraphEdgeCommit> {
        validate_edge_type(edge_type)?;
        validate_value("edge value", value)?;
        let snapshot = self.vault.latest_seq();
        self.require_node(snapshot, src)?;
        self.require_node(snapshot, dst)?;
        let edge_key = self.edge_out_key(src, edge_type, dst)?;
        let reverse_key = self.edge_in_key(dst, edge_type, src)?;
        let seq = self.vault.write_cf_batch([
            (ColumnFamily::Graph, edge_key.clone(), value.to_vec()),
            (ColumnFamily::Graph, reverse_key.clone(), edge_key.clone()),
        ])?;
        Ok(GraphEdgeCommit {
            seq,
            edge_key,
            reverse_key,
        })
    }

    pub fn get_node(&self, snapshot: Seq, node: CxId) -> Result<Option<Vec<u8>>> {
        self.vault
            .read_cf_at(snapshot, ColumnFamily::Graph, &self.node_key(node))
    }

    pub fn get_edge(
        &self,
        snapshot: Seq,
        src: CxId,
        edge_type: &str,
        dst: CxId,
    ) -> Result<Option<Vec<u8>>> {
        let key = self.edge_out_key(src, edge_type, dst)?;
        self.vault.read_cf_at(snapshot, ColumnFamily::Graph, &key)
    }

    /// Enumerates every physical row that must be tombstoned when `node` is
    /// erased. Incident edges are stored in forward and reverse forms, and all
    /// collection-local CSR/metadata projections are invalidated if any live
    /// graph row changes so they cannot retain the erased context indirectly.
    pub(crate) fn erasure_keys_for_node(&self, snapshot: Seq, node: CxId) -> Result<Vec<Vec<u8>>> {
        let mut keys = BTreeSet::new();
        let node_key = self.node_key(node);
        if self
            .vault
            .read_cf_at(snapshot, ColumnFamily::Graph, &node_key)?
            .is_some()
        {
            keys.insert(node_key);
        }

        for key in self.scan_keys_at(snapshot, &self.keys.edge_prefix(true, node, None)?)? {
            let edge = self.keys.decode_edge_out_key(&key)?;
            keys.insert(key);
            let reverse = self.keys.edge_in_key(edge.dst, &edge.edge_type, edge.src)?;
            if self
                .vault
                .read_cf_at(snapshot, ColumnFamily::Graph, &reverse)?
                .is_some()
            {
                keys.insert(reverse);
            }
        }

        for key in self.scan_keys_at(snapshot, &self.keys.edge_prefix(false, node, None)?)? {
            let edge = self.keys.decode_edge_in_key(&key)?;
            keys.insert(key);
            let forward = self
                .keys
                .edge_out_key(edge.src, &edge.edge_type, edge.dst)?;
            if self
                .vault
                .read_cf_at(snapshot, ColumnFamily::Graph, &forward)?
                .is_some()
            {
                keys.insert(forward);
            }
        }

        if keys.is_empty() {
            return Ok(Vec::new());
        }
        let csr_key = self.keys.csr_key();
        if self
            .vault
            .read_cf_at(snapshot, ColumnFamily::Graph, &csr_key)?
            .is_some()
        {
            keys.insert(csr_key);
        }
        keys.extend(self.scan_keys_at(snapshot, &self.keys.csr_segment_range())?);
        keys.extend(self.scan_keys_at(snapshot, &self.keys.metadata_range())?);
        Ok(keys.into_iter().collect())
    }

    pub fn out_neighbors(
        &self,
        snapshot: Seq,
        src: CxId,
        edge_type: Option<&str>,
        limit: usize,
    ) -> Result<Vec<PlainGraphEdge>> {
        self.neighbors(snapshot, src, edge_type, PlainGraphDirection::Out, limit)
    }

    pub fn in_neighbors(
        &self,
        snapshot: Seq,
        dst: CxId,
        edge_type: Option<&str>,
        limit: usize,
    ) -> Result<Vec<PlainGraphEdge>> {
        self.neighbors(snapshot, dst, edge_type, PlainGraphDirection::In, limit)
    }

    pub fn traverse(&self, snapshot: Seq, start: CxId, opts: TraverseOptions) -> Result<Vec<CxId>> {
        if opts.max_hops > MAX_TRAVERSE_HOPS || opts.cost_cap == 0 {
            return Err(graph_limit(format!(
                "max_hops={} cost_cap={} exceeds graph traversal bounds",
                opts.max_hops, opts.cost_cap
            )));
        }
        if let Some(edge_type) = opts.edge_type {
            validate_edge_type(edge_type)?;
        }
        self.require_node(snapshot, start)?;
        let mut visited = BTreeSet::from([start]);
        let mut reached = BTreeSet::new();
        let mut frontier = BTreeSet::from([start]);
        let mut cost = 0usize;
        let cap = opts.cost_cap.min(MAX_TRAVERSE_COST);
        for _ in 0..opts.max_hops {
            if frontier.is_empty() {
                break;
            }
            let mut next = BTreeSet::new();
            for node in &frontier {
                let neighbors =
                    self.neighbor_ids(snapshot, *node, opts.edge_type, opts.direction)?;
                for neighbor in neighbors {
                    cost += 1;
                    if cost > cap {
                        return Err(graph_limit(format!(
                            "graph traversal scanned more than {cap} edge rows"
                        )));
                    }
                    if visited.insert(neighbor) {
                        reached.insert(neighbor);
                        next.insert(neighbor);
                    }
                }
            }
            frontier = next;
        }
        Ok(reached.into_iter().collect())
    }

    pub fn csr_projection(&self, snapshot: Seq) -> Result<PlainGraphCsr> {
        self.csr_projection_with_weight_policy(snapshot, EdgeWeightPolicy::Strict)
            .map(|(projection, _stats)| projection)
    }

    fn csr_projection_with_weight_policy(
        &self,
        snapshot: Seq,
        weight_policy: EdgeWeightPolicy,
    ) -> Result<(PlainGraphCsr, PlainGraphEdgeWeightStats)> {
        let nodes = self.node_ids(snapshot)?;
        let node_index = nodes
            .iter()
            .enumerate()
            .map(|(index, id)| (*id, index))
            .collect::<BTreeMap<_, _>>();
        let mut builder = AssocGraph::builder();
        for node in &nodes {
            builder.add_node(*node, 1.0).map_err(path_error)?;
        }
        let mut stats = PlainGraphEdgeWeightStats::default();
        let mut drafts = Vec::new();
        let mut max_raw_weight = 0.0_f32;
        for key in self.scan_keys_at(snapshot, &self.keys.edge_out_range())? {
            let edge = self.keys.decode_edge_out_key(&key)?;
            let src = *node_index.get(&edge.src).ok_or_else(|| {
                graph_corrupt(format!("edge source {} has no node row", edge.src))
            })?;
            if !node_index.contains_key(&edge.dst) {
                return Err(graph_corrupt(format!(
                    "edge destination {} has no node row",
                    edge.dst
                )));
            }
            let value = self
                .vault
                .read_cf_at(snapshot, ColumnFamily::Graph, &key)?
                .ok_or_else(|| graph_corrupt("graph edge row disappeared during CSR projection"))?;
            let parsed = plain_graph_edge_raw_weight_with_policy(&value, weight_policy)?;
            match parsed.source {
                EdgeWeightSource::Explicit => stats.explicit_weight_edges += 1,
                EdgeWeightSource::LegacyUnit => stats.legacy_unit_weight_edges += 1,
            }
            max_raw_weight = max_raw_weight.max(parsed.raw);
            drafts.push(WeightedEdgeDraft {
                src: edge.src,
                src_index: src,
                dst: edge.dst,
                edge_type: edge.edge_type,
                raw_weight: parsed.raw,
            });
        }
        let mut by_src = vec![Vec::<PlainGraphCsrEdge>::new(); nodes.len()];
        for draft in drafts {
            let weight = plain_graph_normalized_edge_weight(draft.raw_weight, max_raw_weight)?;
            builder
                .add_edge(draft.src, draft.dst, weight)
                .map_err(path_error)?;
            by_src[draft.src_index].push(PlainGraphCsrEdge {
                dst: draft.dst,
                edge_type: draft.edge_type,
                weight,
            });
        }
        let association_edge_count = builder.build().edge_count();
        let (offsets, edges) = flatten_csr_edges(by_src);
        let projection = PlainGraphCsr {
            collection: self.keys.collection_name(),
            source_snapshot: snapshot,
            nodes,
            offsets,
            edges,
            association_edge_count,
        };
        Ok((projection, stats))
    }

    /// Persist the collection-local CSR projection as ordered byte segments plus
    /// a manifest.
    ///
    /// This scans only this `PlainGraph` collection's node and outgoing-edge
    /// key ranges; unrelated Graph CF collections are outside the key prefixes
    /// used by `self.keys`.
    ///
    pub fn rebuild_csr(&self, snapshot: Seq) -> Result<CsrCommit> {
        let projection = self.csr_projection(snapshot)?;
        self.write_csr_projection(projection)
    }

    pub fn rebuild_csr_with_legacy_unit_weights(
        &self,
        snapshot: Seq,
    ) -> Result<(CsrCommit, PlainGraphEdgeWeightStats)> {
        let (projection, stats) =
            self.csr_projection_with_weight_policy(snapshot, EdgeWeightPolicy::LegacyUnit)?;
        self.write_csr_projection(projection)
            .map(|commit| (commit, stats))
    }

    pub fn write_csr_projection(&self, projection: PlainGraphCsr) -> Result<CsrCommit> {
        if projection.collection != self.keys.collection_name() {
            return Err(graph_corrupt(format!(
                "CSR projection collection {} does not match graph collection {}",
                projection.collection,
                self.keys.collection_name()
            )));
        }
        let (manifest_bytes, segments) = csr_store::encode_csr_segments(&self.keys, &projection)?;
        for (ordinal, segment) in segments.into_iter().enumerate() {
            self.vault.write_cf(
                ColumnFamily::Graph,
                self.keys.csr_segment_key(ordinal as u32),
                segment,
            )?;
        }
        let key = self.keys.csr_key();
        let seq = self
            .vault
            .write_cf(ColumnFamily::Graph, key.clone(), manifest_bytes)?;
        Ok(CsrCommit {
            seq,
            key,
            projection,
        })
    }

    pub fn read_csr(&self, snapshot: Seq) -> Result<Option<PlainGraphCsr>> {
        csr_store::load_csr(&self.keys, |key| {
            self.vault.read_cf_at(snapshot, ColumnFamily::Graph, key)
        })
    }

    pub fn assoc_graph(&self, snapshot: Seq) -> Result<AssocGraph> {
        if let Some(csr) = self.read_csr(snapshot)? {
            return assoc_graph_from_csr(&csr);
        }
        let nodes = self.node_ids(snapshot)?;
        let node_set = nodes.iter().copied().collect::<BTreeSet<_>>();
        let mut builder = AssocGraph::builder();
        for node in &nodes {
            builder.add_node(*node, 1.0).map_err(path_error)?;
        }
        let mut drafts = Vec::new();
        let mut max_raw_weight = 0.0_f32;
        for key in self.scan_keys_at(snapshot, &self.keys.edge_out_range())? {
            let edge = self.keys.decode_edge_out_key(&key)?;
            if !node_set.contains(&edge.src) || !node_set.contains(&edge.dst) {
                return Err(graph_corrupt("graph edge endpoint has no node row"));
            }
            let value = self
                .vault
                .read_cf_at(snapshot, ColumnFamily::Graph, &key)?
                .ok_or_else(|| graph_corrupt("graph edge row disappeared during assoc scan"))?;
            let raw_weight = plain_graph_edge_raw_weight(&value)?;
            max_raw_weight = max_raw_weight.max(raw_weight);
            drafts.push(WeightedEdgeDraft {
                src: edge.src,
                src_index: 0,
                dst: edge.dst,
                edge_type: edge.edge_type,
                raw_weight,
            });
        }
        for draft in drafts {
            let weight = plain_graph_normalized_edge_weight(draft.raw_weight, max_raw_weight)?;
            builder
                .add_edge(draft.src, draft.dst, weight)
                .map_err(path_error)?;
        }
        Ok(builder.build())
    }

    pub fn put_metadata(&self, name: &str, value: &[u8]) -> Result<Seq> {
        validate_value("metadata value", value)?;
        self.vault.write_cf(
            ColumnFamily::Graph,
            self.keys.metadata_key(name)?,
            value.to_vec(),
        )
    }

    pub fn get_metadata(&self, snapshot: Seq, name: &str) -> Result<Option<Vec<u8>>> {
        self.vault.read_cf_at(
            snapshot,
            ColumnFamily::Graph,
            &self.keys.metadata_key(name)?,
        )
    }

    pub fn node_key(&self, node: CxId) -> Vec<u8> {
        self.keys.node_key(node)
    }

    pub fn edge_out_key(&self, src: CxId, edge_type: &str, dst: CxId) -> Result<Vec<u8>> {
        self.keys.edge_out_key(src, edge_type, dst)
    }

    pub fn edge_in_key(&self, dst: CxId, edge_type: &str, src: CxId) -> Result<Vec<u8>> {
        self.keys.edge_in_key(dst, edge_type, src)
    }

    fn neighbors(
        &self,
        snapshot: Seq,
        node: CxId,
        edge_type: Option<&str>,
        direction: PlainGraphDirection,
        limit: usize,
    ) -> Result<Vec<PlainGraphEdge>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        if let Some(edge_type) = edge_type {
            validate_edge_type(edge_type)?;
        }
        let mut out = Vec::new();
        if matches!(
            direction,
            PlainGraphDirection::Out | PlainGraphDirection::Both
        ) {
            self.push_out_neighbors(snapshot, node, edge_type, limit, &mut out)?;
        }
        if matches!(
            direction,
            PlainGraphDirection::In | PlainGraphDirection::Both
        ) {
            self.push_in_neighbors(snapshot, node, edge_type, limit, &mut out)?;
        }
        Ok(out)
    }

    fn push_out_neighbors(
        &self,
        snapshot: Seq,
        node: CxId,
        edge_type: Option<&str>,
        limit: usize,
        out: &mut Vec<PlainGraphEdge>,
    ) -> Result<()> {
        for (key, value) in
            self.scan_at(snapshot, &self.keys.edge_prefix(true, node, edge_type)?)?
        {
            if out.len() >= limit {
                return Err(graph_limit(format!("neighbor scan exceeded limit {limit}")));
            }
            let edge = self.keys.decode_edge_out_key(&key)?;
            out.push(PlainGraphEdge {
                src: edge.src,
                dst: edge.dst,
                edge_type: edge.edge_type,
                value,
            });
        }
        Ok(())
    }

    fn push_in_neighbors(
        &self,
        snapshot: Seq,
        node: CxId,
        edge_type: Option<&str>,
        limit: usize,
        out: &mut Vec<PlainGraphEdge>,
    ) -> Result<()> {
        for (key, forward_key) in
            self.scan_at(snapshot, &self.keys.edge_prefix(false, node, edge_type)?)?
        {
            if out.len() >= limit {
                return Err(graph_limit(format!("neighbor scan exceeded limit {limit}")));
            }
            let edge = self.keys.decode_edge_in_key(&key)?;
            let value = self
                .vault
                .read_cf_at(snapshot, ColumnFamily::Graph, &forward_key)?
                .ok_or_else(|| graph_corrupt("reverse graph edge points at missing row"))?;
            out.push(PlainGraphEdge {
                src: edge.src,
                dst: edge.dst,
                edge_type: edge.edge_type,
                value,
            });
        }
        Ok(())
    }

    fn neighbor_ids(
        &self,
        snapshot: Seq,
        node: CxId,
        edge_type: Option<&str>,
        direction: PlainGraphDirection,
    ) -> Result<Vec<CxId>> {
        Ok(self
            .neighbors(snapshot, node, edge_type, direction, MAX_TRAVERSE_COST)?
            .into_iter()
            .map(|edge| if edge.src == node { edge.dst } else { edge.src })
            .collect())
    }

    fn node_ids(&self, snapshot: Seq) -> Result<Vec<CxId>> {
        self.scan_keys_at(snapshot, &self.keys.node_range())?
            .into_iter()
            .map(|key| self.keys.decode_node_key(&key))
            .collect()
    }

    fn require_node(&self, snapshot: Seq, node: CxId) -> Result<()> {
        self.get_node(snapshot, node)?.map_or_else(
            || Err(graph_missing(format!("graph node {node} is absent"))),
            |_| Ok(()),
        )
    }

    fn scan_at(&self, snapshot: Seq, range: &KeyRange) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        self.vault
            .scan_cf_range_at(snapshot, ColumnFamily::Graph, range)
    }

    fn scan_keys_at(&self, snapshot: Seq, range: &KeyRange) -> Result<Vec<Vec<u8>>> {
        self.vault
            .scan_cf_range_keys_at(snapshot, ColumnFamily::Graph, range)
    }
}
