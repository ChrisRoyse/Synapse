//! Engine trait boundaries shared by Calyx crates.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{
    Anchor, Constellation, CxId, LensId, Modality, Result, Seq, Signal, SlotShape, SlotVector,
};

/// Raw input presented to a frozen lens.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Input {
    /// Input modality.
    pub modality: Modality,
    /// Raw bytes to measure.
    pub bytes: Vec<u8>,
    /// Optional pointer to retained source bytes.
    pub pointer: Option<String>,
}

impl Input {
    /// Builds an input from modality and bytes.
    pub fn new(modality: Modality, bytes: impl Into<Vec<u8>>) -> Self {
        Self {
            modality,
            bytes: bytes.into(),
            pointer: None,
        }
    }

    /// Attaches a source pointer.
    pub fn with_pointer(mut self, pointer: impl Into<String>) -> Self {
        self.pointer = Some(pointer.into());
        self
    }
}

/// Exact identity of a runtime that can measure multiple frozen lens
/// projections in one forward pass.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MeasurementGroupKey([u8; 32]);

impl MeasurementGroupKey {
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

/// One independently frozen projection requested from a grouped runtime.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GroupedLensRequest {
    pub lens_id: LensId,
    pub shape: SlotShape,
}

/// Implemented by Registry lens runtimes as frozen measurement instruments.
pub trait Lens: Send + Sync {
    /// Stable frozen lens id.
    fn id(&self) -> LensId;

    /// Vector shape this lens emits.
    fn shape(&self) -> SlotShape;

    /// Modality this lens accepts.
    fn modality(&self) -> Modality;

    /// Whether a free-text query string can be measured through this lens and
    /// produce a vector comparable to the vectors this lens produced at ingest.
    ///
    /// This is deliberately **not** an inference from [`Self::modality`]. The
    /// modality tag is coarse — one-hot encoders, cyclic-time encoders, numeric
    /// scalars and sparse text encoders can all share a single tag — so gating
    /// text recall on it either admits lenses that cannot answer a text query
    /// (a timestamp encoder handed the word "focus_change") or, when a whole
    /// panel declares itself structured, excludes every lens including the
    /// sparse text lane the recall path is built on. The latter is what shipped:
    /// `Modality::Text` appeared zero times across the 78 Synapse lens
    /// declarations, so `by_text` could never produce a query vector on any
    /// Synapse panel regardless of index state (issue #1896).
    ///
    /// Text-queryability is a property of the *encoder*: it is true exactly when
    /// measuring the query bytes through this lens is the same operation that
    /// measured the stored bytes, which is the symmetry a lexical or dense
    /// retrieval lane requires to compare a query against a document at all.
    ///
    /// The default preserves the historical gate for every runtime that has not
    /// declared otherwise, so this can only widen recall, never narrow it.
    fn text_queryable(&self) -> bool {
        self.modality() == Modality::Text
    }

    /// Deterministically measures one input.
    fn measure(&self, input: &Input) -> Result<SlotVector>;

    /// Deterministically measures a batch of inputs.
    fn measure_batch(&self, inputs: &[Input]) -> Result<Vec<SlotVector>> {
        inputs.iter().map(|input| self.measure(input)).collect()
    }

    /// Returns an exact multi-output runtime identity when this lens supports
    /// grouped measurement. The default preserves standalone lens behavior.
    fn measurement_group_key(&self) -> Result<Option<MeasurementGroupKey>> {
        Ok(None)
    }

    /// Measures compatible projections in one runtime call. `None` means this
    /// lens does not implement grouped output; callers must not silently retry
    /// projections independently after a grouped failure.
    fn measure_grouped_batch(
        &self,
        _requests: &[GroupedLensRequest],
        _inputs: &[Input],
    ) -> Result<Option<BTreeMap<LensId, Vec<SlotVector>>>> {
        Ok(None)
    }
}

/// Implemented by per-slot ANN or inverted indexes.
pub trait Index: Send + Sync {
    /// Inserts or replaces a vector for a constellation.
    fn insert(&mut self, cx: CxId, vector: &SlotVector) -> Result<()>;

    /// Searches for nearest constellations.
    fn search(&self, query: &SlotVector, k: usize, ef: Option<usize>) -> Result<Vec<(CxId, f32)>>;

    /// Rebuilds the index from its source store.
    fn rebuild(&mut self) -> Result<()>;
}

/// Implemented by Aster vault storage.
pub trait VaultStore: Send + Sync {
    /// Persists a constellation through the group-commit path.
    fn put(&self, constellation: Constellation) -> Result<CxId>;

    /// Reads a constellation as of a snapshot sequence.
    fn get(&self, id: CxId, snapshot: Seq) -> Result<Constellation>;

    /// Attaches a grounded anchor to an existing constellation.
    fn anchor(&self, id: CxId, anchor: Anchor) -> Result<()>;

    /// Returns the latest readable snapshot sequence.
    fn snapshot(&self) -> Seq;
}

/// Implemented by Assay signal estimators.
pub trait Estimator: Send + Sync {
    /// Estimates information between slot vectors and anchors.
    fn mi(&self, x: &[SlotVector], y: &[Anchor]) -> Result<Signal>;

    /// Estimates redundancy between two slot-vector samples.
    fn redundancy(&self, a: &[SlotVector], b: &[SlotVector]) -> Result<f32>;
}
