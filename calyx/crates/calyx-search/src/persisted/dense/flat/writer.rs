use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use calyx_core::{CxId, SlotId};

use super::{FORMAT, Header, write_header};
use crate::error::{CliError, CliResult};
use crate::persisted::dense::DenseSlotRows;
use crate::persisted::{SearchIndexEntry, rel, stale, write_atomic_hashed};

pub(in crate::persisted) struct StreamingWriter {
    vault_dir: PathBuf,
    path: PathBuf,
    temporary_path: PathBuf,
    slot: SlotId,
    dim: u32,
    base_seq: u64,
    expected_len: usize,
    row_count: usize,
    last_cx_id: Option<CxId>,
    writer: Option<BufWriter<File>>,
}

impl StreamingWriter {
    pub(in crate::persisted) fn new(
        vault_dir: &Path,
        root: &Path,
        slot: SlotId,
        dim: u32,
        base_seq: u64,
        expected_len: usize,
    ) -> CliResult<Self> {
        fs::create_dir_all(root)?;
        let temporary_path = root.join(format!(
            "slot_{:05}_seq_{base_seq:020}.flatdense.rows.tmp",
            slot.get()
        ));
        let writer = File::create(&temporary_path)
            .map(BufWriter::new)
            .map_err(CliError::from)
            .map_err(|primary| cleanup_error(&temporary_path, primary))?;
        Ok(Self {
            vault_dir: vault_dir.to_path_buf(),
            path: root.to_path_buf(),
            temporary_path,
            slot,
            dim,
            base_seq,
            expected_len,
            row_count: 0,
            last_cx_id: None,
            writer: Some(writer),
        })
    }

    pub(in crate::persisted) fn push(&mut self, cx_id: CxId, values: &[f32]) -> CliResult {
        crate::persisted::dense::validate_dense(self.slot, cx_id, self.dim, values)?;
        if self.row_count >= self.expected_len {
            return Err(stale(format!(
                "streaming flat dense slot {} received more than its declared {} rows",
                self.slot, self.expected_len
            )));
        }
        if self.last_cx_id.is_some_and(|previous| previous >= cx_id) {
            return Err(stale(format!(
                "streaming flat dense slot {} row order is not strictly increasing: previous={:?}, current={cx_id}",
                self.slot, self.last_cx_id
            )));
        }
        let writer = self
            .writer
            .as_mut()
            .ok_or_else(|| stale("streaming flat dense writer is already finalized"))?;
        writer.write_all(cx_id.as_bytes())?;
        for value in values {
            writer.write_all(&value.to_le_bytes())?;
        }
        self.row_count = self
            .row_count
            .checked_add(1)
            .ok_or_else(|| stale("streaming flat dense row count overflow"))?;
        self.last_cx_id = Some(cx_id);
        Ok(())
    }

    pub(in crate::persisted) fn finish(mut self) -> CliResult<SearchIndexEntry> {
        let path = self.path.join(format!(
            "slot_{:05}_seq_{:020}_n_{:010}.flatdense.bin",
            self.slot.get(),
            self.base_seq,
            self.row_count
        ));
        let index_rel = rel(&self.vault_dir, &path)?;
        let writer = self
            .writer
            .take()
            .ok_or_else(|| stale("streaming flat dense writer is already finalized"))?;
        let finalize_result: CliResult = (|| {
            let file = writer.into_inner().map_err(|error| {
                CliError::io(format!(
                    "flush flat dense sidecar {}: {error}",
                    self.temporary_path.display()
                ))
            })?;
            file.sync_all()?;
            Ok(())
        })();
        finalize_result.map_err(|primary| cleanup_error(&self.temporary_path, primary))?;
        let header = Header {
            format: FORMAT.to_string(),
            slot: self.slot.get(),
            dim: self.dim,
            base_seq: self.base_seq,
            len: self.row_count,
        };
        let write_result = write_atomic_hashed(&path, |writer| {
            write_header(writer, &header)?;
            let mut rows = File::open(&self.temporary_path)?;
            std::io::copy(&mut rows, writer)?;
            Ok(())
        });
        let sha256 = match write_result {
            Ok(sha256) => sha256,
            Err(primary) => return Err(cleanup_error(&self.temporary_path, primary)),
        };
        if let Err(cleanup) = remove_temporary(&self.temporary_path) {
            let published_cleanup = fs::remove_file(&path);
            return Err(stale(format!(
                "published flat dense sidecar {} but row-stream cleanup failed [{}] {}; published cleanup result={published_cleanup:?}",
                path.display(),
                cleanup.code(),
                cleanup.message()
            )));
        }
        Ok(SearchIndexEntry::flat_dense(
            self.slot,
            self.dim,
            self.row_count,
            self.base_seq,
            index_rel,
            sha256,
        ))
    }

    pub(in crate::persisted) fn abort(mut self) -> CliResult {
        self.writer.take();
        remove_temporary(&self.temporary_path)
    }
}

impl Drop for StreamingWriter {
    fn drop(&mut self) {
        if self.writer.take().is_some() {
            let _ = remove_temporary(&self.temporary_path);
        }
    }
}

pub(in crate::persisted) fn write(
    vault_dir: &Path,
    root: &Path,
    slot: SlotId,
    rows: DenseSlotRows,
    base_seq: u64,
) -> CliResult<SearchIndexEntry> {
    let mut writer =
        StreamingWriter::new(vault_dir, root, slot, rows.dim, base_seq, rows.rows.len())?;
    for (cx_id, values) in rows.rows {
        writer.push(cx_id, &values)?;
    }
    writer.finish()
}

fn remove_temporary(path: &Path) -> CliResult {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(stale(format!(
            "remove partial flat dense sidecar {} failed: {error}",
            path.display()
        ))),
    }
}

fn cleanup_error(path: &Path, primary: CliError) -> CliError {
    match remove_temporary(path) {
        Ok(()) => primary,
        Err(cleanup) => stale(format!(
            "flat dense sidecar operation failed [{}] {}; temporary cleanup also failed [{}] {}",
            primary.code(),
            primary.message(),
            cleanup.code(),
            cleanup.message()
        )),
    }
}
