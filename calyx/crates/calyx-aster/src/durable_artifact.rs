//! Durable publication primitives for immutable derived artifacts.
//!
//! Derived indexes should stream immutable generation bytes, validate a
//! read-only mapping, and only then replace their small current-generation
//! pointer. These wrappers keep that crash boundary identical to Aster's own
//! manifest/SST publication without exposing the internal filesystem module.

use std::fs::File;
use std::io;
use std::path::Path;

use calyx_core::Result;

/// Streams and durably publishes one create-new immutable generation.
pub fn publish_immutable_with(
    path: &Path,
    label: &str,
    write: impl FnOnce(&mut File) -> io::Result<()>,
) -> Result<()> {
    crate::fsync::write_atomic_create_new_stream(path, label, write)
}

/// Atomically and durably replaces a small pointer to an immutable generation.
pub fn publish_current(path: &Path, bytes: &[u8], label: &str) -> Result<()> {
    crate::fsync::write_atomic_replace(path, bytes, label)
}

/// Removes an obsolete immutable generation and syncs the parent directory.
pub fn remove_obsolete(path: &Path, label: &str) -> Result<()> {
    crate::fsync::remove_file_durable(path, label)
}
