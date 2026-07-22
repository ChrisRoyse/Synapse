//! Packed raw-f32 vector sidecar (`graph.raw`), format v2 (#1990).
//!
//! Historically the raw sidecar was a directory holding one 4·dim-byte file per
//! node (`graph.raw/<id>`). On a 109k-row slot that is ~109k inodes per slot
//! (13 slots ≈ 1.42M files): inode/journal bloat, no write-time durability
//! (per-file `fs::write` never fsynced; publish was a bare dir rename), and an
//! `O(n)` `statx` walk on every vault open (#1989/#1990).
//!
//! v2 packs the whole sidecar into one regular file at the same `graph.raw`
//! path: a fixed [`RAW_SIDECAR_HEADER_SIZE`]-byte header followed by
//! `node_count` contiguous `dim·4`-byte little-endian f32 records. Node `id`'s
//! record lives at byte offset `RAW_SIDECAR_HEADER_SIZE + id·dim·4`, so lookup
//! is `O(1)` — the same complexity as the old `dir.join(id)` open, minus an
//! inode per node. Ids are dense `0..node_count` (graph build renumbers
//! densely); the writer proves that invariant and fails closed otherwise.
//!
//! The writer streams records into a `.tmp` sibling, issues one `sync_all`, and
//! atomically renames into place — the exact durability contract of
//! [`super::super::graph::DiskAnnGraphWriter::finish`]. The file is created
//! `0600` via the #1989 `private_fs` primitives.
//!
//! Readers dispatch on the on-disk node type of `graph.raw`: a **regular file**
//! is packed v2; a **directory** is legacy v1 (existing production vaults, which
//! migrate to v2 on their next index rebuild). Anything else — a symlink, or a
//! required-but-missing sidecar — fails closed with a typed `CALYX_*` error. A
//! short/corrupt packed file fails closed on open; readers never zero-fill.

use std::fs::{self, File};
use std::io::{BufWriter, Write as _};
use std::path::{Path, PathBuf};

use calyx_core::Result;
use memmap2::Mmap;

use super::helpers::{invalid, io};
use crate::error::{CALYX_INDEX_CORRUPT, CALYX_INDEX_DIM_MISMATCH, CALYX_INDEX_IO, sextant_error};
use crate::index::diskann::graph::DISKANN_MAX_DIM;

/// Magic at offset 0 of a packed v2 `graph.raw` file.
pub const RAW_SIDECAR_MAGIC: [u8; 8] = *b"CLXRAW01";
/// Packed layout version stored in the header (`v1` was the directory layout).
pub const RAW_SIDECAR_PACKED_VERSION: u32 = 2;
/// Fixed header size in bytes. The 24-byte prefix (magic + version + dim +
/// node_count) is zero-padded to this size; keeping it a multiple of 4 leaves
/// every record 4-byte aligned within the map.
pub const RAW_SIDECAR_HEADER_SIZE: usize = 64;

fn corrupt(detail: impl std::fmt::Display) -> calyx_core::CalyxError {
    sextant_error(
        CALYX_INDEX_CORRUPT,
        format!("packed raw sidecar corrupt: {detail}"),
    )
}

/// Decoded packed header fields.
struct PackedHeader {
    dim: u32,
    node_count: u64,
}

fn decode_header(block: &[u8]) -> Result<PackedHeader> {
    if block.len() < 24 {
        return Err(corrupt("header shorter than 24 bytes"));
    }
    if block[0..8] != RAW_SIDECAR_MAGIC {
        return Err(corrupt(format!("bad magic {:02x?}", &block[0..8])));
    }
    let version = u32::from_le_bytes(block[8..12].try_into().expect("4B"));
    if version != RAW_SIDECAR_PACKED_VERSION {
        return Err(corrupt(format!(
            "version {version} != {RAW_SIDECAR_PACKED_VERSION}"
        )));
    }
    let dim = u32::from_le_bytes(block[12..16].try_into().expect("4B"));
    if dim == 0 || dim as usize > DISKANN_MAX_DIM {
        return Err(corrupt(format!("dim {dim} out of 1..={DISKANN_MAX_DIM}")));
    }
    let node_count = u64::from_le_bytes(block[16..24].try_into().expect("8B"));
    Ok(PackedHeader { dim, node_count })
}

/// The on-disk shape of a configured `graph.raw` path, resolved once per read
/// operation. Constructing a [`Self::Packed`] validates the packed header and
/// exact file length; a truncated/corrupt file fails closed here.
pub(super) enum RawSidecarSource {
    /// No sidecar path is configured on the index at all.
    NotConfigured,
    /// A sidecar path is configured but nothing exists at it.
    Absent,
    /// Packed v2 regular file, mmap-backed and header-validated.
    Packed(PackedRawSidecar),
    /// Legacy v1 directory of per-node files; reads route through the caller
    /// (it needs the index id map for the historical file-name variants).
    LegacyDir(PathBuf),
}

impl RawSidecarSource {
    /// Whether this source can actually serve raw vectors (packed file or
    /// legacy dir), as opposed to being unconfigured/absent.
    pub(super) fn has_vectors(&self) -> bool {
        matches!(self, Self::Packed(_) | Self::LegacyDir(_))
    }
}

/// Classify a configured (or unconfigured) `graph.raw` path, failing closed on
/// a symlink, a non-file/non-dir node, or a corrupt packed file. A missing path
/// resolves to [`RawSidecarSource::Absent`] (the caller decides whether that is
/// an error) — `symlink_metadata` never follows the leaf, mirroring the
/// `O_NOFOLLOW` contract of the writers (#1989).
pub(super) fn classify(path: Option<&Path>, expected_dim: u32) -> Result<RawSidecarSource> {
    let Some(path) = path else {
        return Ok(RawSidecarSource::NotConfigured);
    };
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(RawSidecarSource::Absent),
        Err(error) => Err(io("stat raw sidecar", error)),
        Ok(meta) => {
            let file_type = meta.file_type();
            if file_type.is_symlink() {
                Err(sextant_error(
                    CALYX_INDEX_IO,
                    format!(
                        "raw sidecar {} is a symlink; refusing to follow",
                        path.display()
                    ),
                ))
            } else if file_type.is_dir() {
                Ok(RawSidecarSource::LegacyDir(path.to_path_buf()))
            } else if file_type.is_file() {
                Ok(RawSidecarSource::Packed(PackedRawSidecar::open(
                    path,
                    expected_dim,
                )?))
            } else {
                Err(sextant_error(
                    CALYX_INDEX_IO,
                    format!(
                        "raw sidecar {} is neither a regular file nor a directory",
                        path.display()
                    ),
                ))
            }
        }
    }
}

/// mmap-backed reader over a packed v2 `graph.raw`. The header and total length
/// are validated on open; per-read the id is bounds-checked against the header
/// `node_count` and every f32 is checked finite.
pub(super) struct PackedRawSidecar {
    mmap: Mmap,
    dim: usize,
    node_count: u64,
}

impl PackedRawSidecar {
    fn open(path: &Path, expected_dim: u32) -> Result<Self> {
        let file = File::open(path).map_err(|e| io("open packed raw sidecar", e))?;
        let len = file
            .metadata()
            .map_err(|e| io("stat packed raw sidecar", e))?
            .len();
        if len < RAW_SIDECAR_HEADER_SIZE as u64 {
            return Err(corrupt(format!(
                "{} is {len} B, smaller than one {RAW_SIDECAR_HEADER_SIZE} B header",
                path.display()
            )));
        }
        // SAFETY: read-only map of a file Calyx publishes atomically via
        // tmp+rename and never mutates in place; truncation mid-read would be an
        // external violation of the vault's exclusive index ownership.
        let mmap = unsafe { Mmap::map(&file).map_err(|e| io("mmap packed raw sidecar", e))? };
        let header = decode_header(&mmap[..RAW_SIDECAR_HEADER_SIZE])?;
        if header.dim != expected_dim {
            return Err(sextant_error(
                CALYX_INDEX_DIM_MISMATCH,
                format!(
                    "packed raw sidecar {} dim {} != index dim {expected_dim}",
                    path.display(),
                    header.dim
                ),
            ));
        }
        let record = header.dim as u64 * 4;
        let expected = RAW_SIDECAR_HEADER_SIZE as u64 + header.node_count * record;
        if len != expected {
            return Err(corrupt(format!(
                "{} is {len} B, expected {expected} B ({} x {record} B records)",
                path.display(),
                header.node_count
            )));
        }
        Ok(Self {
            mmap,
            dim: header.dim as usize,
            node_count: header.node_count,
        })
    }

    /// Number of records the header promises; used to prove density against the
    /// index id map before a bulk read.
    pub(super) fn node_count(&self) -> u64 {
        self.node_count
    }

    /// Decode node `id`'s record into an owned vector, failing closed on an
    /// out-of-bounds id or any non-finite component (never zero-filling).
    pub(super) fn read_vector(&self, id: u32) -> Result<Vec<f32>> {
        if u64::from(id) >= self.node_count {
            return Err(sextant_error(
                CALYX_INDEX_IO,
                format!(
                    "packed raw sidecar node {id} >= node_count {}",
                    self.node_count
                ),
            ));
        }
        let record = self.dim * 4;
        let start = RAW_SIDECAR_HEADER_SIZE + id as usize * record;
        let bytes = &self.mmap[start..start + record];
        let mut out = Vec::with_capacity(self.dim);
        for chunk in bytes.chunks_exact(4) {
            let value = f32::from_le_bytes(chunk.try_into().expect("4B"));
            if !value.is_finite() {
                return Err(sextant_error(
                    CALYX_INDEX_IO,
                    format!("packed raw sidecar node {id} has non-finite f32"),
                ));
            }
            out.push(value);
        }
        Ok(out)
    }
}

/// Remove whatever currently occupies `path` (a legacy v1 directory, a stale v2
/// file, or a leftover tmp) so an atomic rename can publish over it. A symlink
/// is unlinked as a file rather than followed.
fn remove_existing(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => {
            fs::remove_dir_all(path).map_err(|error| io("remove raw sidecar directory", error))?;
        }
        Ok(_) => {
            fs::remove_file(path).map_err(|error| io("remove raw sidecar file", error))?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(io("stat raw sidecar for removal", error)),
    }
    Ok(())
}

fn create_real_dir_all(path: &Path) -> Result<()> {
    fs::create_dir_all(path).map_err(|error| io("create raw sidecar parent", error))?;
    let metadata =
        fs::symlink_metadata(path).map_err(|error| io("stat raw sidecar parent", error))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(sextant_error(
            CALYX_INDEX_IO,
            format!(
                "raw sidecar parent {} is not a real directory",
                path.display()
            ),
        ));
    }
    Ok(())
}

fn open_staging_file(path: &Path) -> Result<File> {
    let metadata = fs::symlink_metadata(path);
    if let Ok(metadata) = &metadata
        && metadata.file_type().is_symlink()
    {
        return Err(sextant_error(
            CALYX_INDEX_IO,
            format!("raw sidecar staging path {} is a symlink", path.display()),
        ));
    }
    if metadata.is_ok() {
        remove_existing(path)?;
    }
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options
        .open(path)
        .map_err(|error| io("create raw sidecar staging file", error))?;
    if !file
        .metadata()
        .map_err(|error| io("stat raw sidecar staging handle", error))?
        .is_file()
    {
        return Err(sextant_error(
            CALYX_INDEX_IO,
            format!(
                "raw sidecar staging path {} is not a regular file",
                path.display()
            ),
        ));
    }
    Ok(file)
}

/// Write the packed v2 sidecar for `rows` (dense ids `0..rows.len()`) to `path`.
///
/// Streams the header + records into a `0600` `.tmp` sibling, issues one
/// `sync_all`, then atomically renames into place — the durability contract of
/// [`DiskAnnGraphWriter::finish`](super::super::graph::DiskAnnGraphWriter). Fails
/// closed if any row is out of dense order or has the wrong dimensionality.
pub(super) fn write_packed(path: &Path, rows: &[(u32, Vec<f32>)], dim: usize) -> Result<()> {
    let dim_u32 = u32::try_from(dim)
        .ok()
        .filter(|d| *d != 0 && *d as usize <= DISKANN_MAX_DIM)
        .ok_or_else(|| {
            invalid(format!(
                "raw sidecar dim {dim} out of 1..={DISKANN_MAX_DIM}"
            ))
        })?;
    let node_count =
        u64::try_from(rows.len()).map_err(|_| invalid("raw sidecar node_count exceeds u64"))?;

    // The packed layout addresses records by id, so ids must be exactly the
    // dense sequence 0..node_count in order. Graph build already renumbers
    // densely; prove it here rather than silently misplacing a record.
    for (expected, (id, vector)) in rows.iter().enumerate() {
        if *id as usize != expected {
            return Err(sextant_error(
                CALYX_INDEX_IO,
                format!(
                    "packed raw sidecar requires dense ids 0..{}; row {expected} has id {id}",
                    rows.len()
                ),
            ));
        }
        if vector.len() != dim {
            return Err(sextant_error(
                CALYX_INDEX_DIM_MISMATCH,
                format!(
                    "raw sidecar vector {id} dim {} expected {dim}",
                    vector.len()
                ),
            ));
        }
    }

    let mut header = [0_u8; RAW_SIDECAR_HEADER_SIZE];
    header[0..8].copy_from_slice(&RAW_SIDECAR_MAGIC);
    header[8..12].copy_from_slice(&RAW_SIDECAR_PACKED_VERSION.to_le_bytes());
    header[12..16].copy_from_slice(&dim_u32.to_le_bytes());
    header[16..24].copy_from_slice(&node_count.to_le_bytes());

    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        create_real_dir_all(parent)?;
    }

    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    // A prior aborted build may have left the tmp as either a file or a legacy
    // directory; clear it before staging.
    remove_existing(&tmp)?;

    // Owner-only (0600) staging file at creation (#1989); one sync_all + rename
    // in this function keeps the same durability contract as the graph writer.
    let file = open_staging_file(&tmp)?;
    let mut out = BufWriter::new(file);
    out.write_all(&header)
        .map_err(|e| io("write raw sidecar header", e))?;
    for (_id, vector) in rows {
        for value in vector {
            out.write_all(&value.to_le_bytes())
                .map_err(|e| io("write raw sidecar record", e))?;
        }
    }
    let file = out
        .into_inner()
        .map_err(|e| io("flush raw sidecar", e.into_error()))?;
    file.sync_all().map_err(|e| io("fsync raw sidecar", e))?;
    drop(file);

    // Publish without destroying the last known-good generation first. Windows
    // cannot rename a file over an existing directory/file uniformly, so move
    // the old generation to a sibling, install the fully fsynced staging file,
    // and restore the old generation if installation fails. A prior interrupted
    // publication is recovered before starting another exchange.
    let mut previous = path.as_os_str().to_owned();
    previous.push(".previous");
    let previous = PathBuf::from(previous);
    if !path.exists() && previous.exists() {
        fs::rename(&previous, path)
            .map_err(|error| io("recover previous raw sidecar generation", error))?;
    }
    remove_existing(&previous)?;
    let had_previous = path.exists();
    if had_previous {
        fs::rename(path, &previous)
            .map_err(|error| io("stage previous raw sidecar generation", error))?;
    }
    if let Err(error) = fs::rename(&tmp, path) {
        if had_previous {
            fs::rename(&previous, path)
                .map_err(|restore| io("restore previous raw sidecar generation", restore))?;
        }
        return Err(io("publish packed raw sidecar", error));
    }
    if had_previous {
        remove_existing(&previous)?;
    }
    Ok(())
}
