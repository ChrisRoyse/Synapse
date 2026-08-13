use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use calyx_core::{CxId, SlotId, SparseEntry};

use super::*;
use crate::error::{CliError, CliResult};
use crate::persisted::{SearchIndexEntry, rel, stale, write_atomic_hashed};

#[derive(Serialize)]
struct SparseRowRef<'a> {
    cx_id: CxId,
    doc_len: f32,
    entries: &'a [SparseEntry],
}

pub(in crate::persisted) struct StreamingWriter {
    vault_dir: PathBuf,
    root: PathBuf,
    row_path: PathBuf,
    slot: SlotId,
    dim: u32,
    base_seq: u64,
    expected_len: usize,
    scoring: SparseScoring,
    row_count: usize,
    field_docs: usize,
    total_doc_len: f32,
    last_cx_id: Option<CxId>,
    writer: Option<BufWriter<File>>,
}

impl StreamingWriter {
    #[allow(clippy::too_many_arguments)]
    pub(in crate::persisted) fn new(
        vault_dir: &Path,
        root: &Path,
        slot: SlotId,
        dim: u32,
        base_seq: u64,
        expected_len: usize,
        scoring: SparseScoring,
    ) -> CliResult<Self> {
        fs::create_dir_all(root)?;
        let row_path = root.join(format!(
            "slot_{:05}_seq_{base_seq:020}_n_{expected_len:010}.sparse.rows.tmp",
            slot.get()
        ));
        let writer = File::create(&row_path)
            .map(BufWriter::new)
            .map_err(CliError::from)
            .map_err(|primary| cleanup_error(&row_path, primary))?;
        Ok(Self {
            vault_dir: vault_dir.to_path_buf(),
            root: root.to_path_buf(),
            row_path,
            slot,
            dim,
            base_seq,
            expected_len,
            scoring,
            row_count: 0,
            field_docs: 0,
            total_doc_len: 0.0,
            last_cx_id: None,
            writer: Some(writer),
        })
    }

    pub(in crate::persisted) fn push(&mut self, cx_id: CxId, entries: &[SparseEntry]) -> CliResult {
        if self.row_count >= self.expected_len {
            return Err(stale(format!(
                "streaming sparse slot {} received more than its declared {} rows",
                self.slot, self.expected_len
            )));
        }
        if self.last_cx_id.is_some_and(|previous| previous >= cx_id) {
            return Err(stale(format!(
                "streaming sparse slot {} row order is not strictly increasing: previous={:?}, current={cx_id}",
                self.slot, self.last_cx_id
            )));
        }
        let doc_len = validate_sparse_weights(entries, self.scoring, &format!("row {cx_id}"))?;
        if !entries.is_empty() {
            self.field_docs = self
                .field_docs
                .checked_add(1)
                .ok_or_else(|| stale("sparse field-document count overflow"))?;
            self.total_doc_len += doc_len;
            if !self.total_doc_len.is_finite() {
                return Err(stale("persistent sparse corpus length overflowed"));
            }
        }
        let writer = self
            .writer
            .as_mut()
            .ok_or_else(|| stale("streaming sparse writer is already finalized"))?;
        serde_json::to_writer(
            &mut *writer,
            &SparseRowRef {
                cx_id,
                doc_len,
                entries,
            },
        )?;
        writer.write_all(b"\n")?;
        self.row_count = self
            .row_count
            .checked_add(1)
            .ok_or_else(|| stale("streaming sparse row count overflow"))?;
        self.last_cx_id = Some(cx_id);
        Ok(())
    }

    pub(in crate::persisted) fn finish(mut self) -> CliResult<SearchIndexEntry> {
        let path = self.root.join(format!(
            "slot_{:05}_seq_{:020}_n_{:010}.sparse.jsonl",
            self.slot.get(),
            self.base_seq,
            self.row_count
        ));
        let index_rel = rel(&self.vault_dir, &path)?;
        let writer = self
            .writer
            .take()
            .ok_or_else(|| stale("streaming sparse writer is already finalized"))?;
        let finalize_result: CliResult = (|| {
            let file = writer.into_inner().map_err(|error| {
                CliError::io(format!(
                    "flush sparse row stream {}: {error}",
                    self.row_path.display()
                ))
            })?;
            file.sync_all()?;
            Ok(())
        })();
        if let Err(primary) = finalize_result {
            return Err(cleanup_error(&self.row_path, primary));
        }
        let avg_doc_len = if self.field_docs == 0 {
            0.0
        } else {
            self.total_doc_len / self.field_docs as f32
        };
        let header = StreamingSparseHeader {
            format: SPARSE_FORMAT_V4.to_owned(),
            scoring: self.scoring,
            slot: self.slot.get(),
            dim: self.dim,
            base_seq: self.base_seq,
            len: self.row_count,
            field_docs: self.field_docs,
            avg_doc_len,
        };
        let write_result = write_atomic_hashed(&path, |writer| {
            serde_json::to_writer(&mut *writer, &header)?;
            writer.write_all(b"\n")?;
            let mut rows = File::open(&self.row_path)?;
            std::io::copy(&mut rows, writer)?;
            Ok(())
        });
        let sha256 = match write_result {
            Ok(sha256) => sha256,
            Err(primary) => return Err(cleanup_error(&self.row_path, primary)),
        };
        if let Err(cleanup) = remove_temporary(&self.row_path) {
            let published_cleanup = fs::remove_file(&path);
            return Err(stale(format!(
                "published sparse sidecar {} but row-stream cleanup failed [{}] {}; published cleanup result={published_cleanup:?}",
                path.display(),
                cleanup.code(),
                cleanup.message()
            )));
        }
        Ok(SearchIndexEntry::sparse(
            self.slot,
            self.dim,
            self.row_count,
            self.base_seq,
            index_rel,
            sha256,
            self.scoring.index_kind(),
        ))
    }

    pub(in crate::persisted) fn abort(mut self) -> CliResult {
        self.writer.take();
        remove_temporary(&self.row_path)
    }
}

impl Drop for StreamingWriter {
    fn drop(&mut self) {
        if self.writer.take().is_some() {
            let _ = remove_temporary(&self.row_path);
        }
    }
}

fn remove_temporary(path: &Path) -> CliResult {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(stale(format!(
            "remove partial sparse row stream {} failed: {error}",
            path.display()
        ))),
    }
}

fn cleanup_error(path: &Path, primary: CliError) -> CliError {
    match remove_temporary(path) {
        Ok(()) => primary,
        Err(cleanup) => stale(format!(
            "sparse row-stream operation failed [{}] {}; temporary cleanup also failed [{}] {}",
            primary.code(),
            primary.message(),
            cleanup.code(),
            cleanup.message()
        )),
    }
}
