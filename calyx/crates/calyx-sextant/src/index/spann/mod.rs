//! SPANN sparse-slot index: centroid ANN in RAM, posting lists on disk.

pub mod centroids;
pub mod posting;

pub use centroids::{
    SPANN_CENTROID_MAGIC, SpannCentroidIndex, build_centroids, try_build_centroids,
};
pub use posting::{PostingListReader, PostingListWriter, PostingMember, SpannSearch};
