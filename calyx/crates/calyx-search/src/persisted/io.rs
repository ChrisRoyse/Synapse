use super::*;

pub(super) fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> CliResult {
    write_bytes_atomic(path, &serde_json::to_vec_pretty(value)?)
}

pub(super) fn write_bytes_atomic(path: &Path, bytes: &[u8]) -> CliResult {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    {
        let mut file = File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    fs::rename(&tmp, path).inspect_err(|_| {
        let _ = fs::remove_file(&tmp);
    })?;
    Ok(())
}

/// Stream `value` as compact JSON straight to `path` (atomic temp + rename), hashing
/// the bytes as they pass through. This avoids materializing the whole serialized
/// sidecar in memory: `to_vec_pretty` over a full multi-vector (ColBERT) corpus builds
/// a multi-gigabyte buffer and pins one core for many minutes formatting billions of
/// floats — the post-ingest finalization hang. Returns the lowercase-hex SHA-256 of the
/// written bytes (computed in the same single pass, no second walk over the buffer).
pub(super) fn write_json_atomic_hashed<T: Serialize>(path: &Path, value: &T) -> CliResult<String> {
    write_atomic_hashed(path, |writer| {
        serde_json::to_writer(writer, value)?;
        Ok(())
    })
}

pub(super) fn write_atomic_hashed<F>(path: &Path, write_fn: F) -> CliResult<String>
where
    F: FnOnce(&mut HashingWriter<BufWriter<File>>) -> CliResult<()>,
{
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    let write_result: CliResult<String> = (|| {
        let file = File::create(&tmp)?;
        let mut writer = HashingWriter::new(BufWriter::new(file));
        write_fn(&mut writer)?;
        let (buf_writer, sha256) = writer.into_parts();
        let file = buf_writer
            .into_inner()
            .map_err(|err| CliError::io(format!("flush index sidecar {}: {err}", tmp.display())))?;
        file.sync_all()?;
        Ok(sha256)
    })();
    let sha256 = match write_result {
        Ok(sha256) => sha256,
        Err(primary) => {
            return match fs::remove_file(&tmp) {
                Ok(()) => Err(primary),
                Err(cleanup) if cleanup.kind() == std::io::ErrorKind::NotFound => Err(primary),
                Err(cleanup) => Err(stale(format!(
                    "streamed atomic sidecar write failed [{}] {}; temporary file cleanup at {} also failed: {cleanup}",
                    primary.code(),
                    primary.message(),
                    tmp.display()
                ))),
            };
        }
    };
    fs::rename(&tmp, path).inspect_err(|_| {
        let _ = fs::remove_file(&tmp);
    })?;
    Ok(sha256)
}

/// `io::Write` adapter that folds every written byte into a SHA-256 digest as it passes
/// through to the inner writer, so a sidecar's hash is produced during the streaming
/// write instead of a separate pass over a fully materialized buffer.
pub(super) struct HashingWriter<W: Write> {
    inner: W,
    hasher: Sha256,
}

impl<W: Write> HashingWriter<W> {
    fn new(inner: W) -> Self {
        Self {
            inner,
            hasher: Sha256::new(),
        }
    }

    fn into_parts(self) -> (W, String) {
        let digest = self.hasher.finalize();
        let mut hex = String::with_capacity(digest.len() * 2);
        for byte in digest {
            hex.push_str(&format!("{byte:02x}"));
        }
        (self.inner, hex)
    }
}

impl<W: Write> Write for HashingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let written = self.inner.write(buf)?;
        self.hasher.update(&buf[..written]);
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// `io::Read` adapter that hashes the exact bytes consumed by a streaming
/// decoder. Validation and decoding therefore share one file handle and one
/// byte stream; replacing an artifact between a hash pass and a parse pass
/// cannot make unverified bytes visible to the caller.
pub(super) struct HashingReader<R: Read> {
    inner: R,
    hasher: Sha256,
}

impl<R: Read> HashingReader<R> {
    pub(super) fn new(inner: R) -> Self {
        Self {
            inner,
            hasher: Sha256::new(),
        }
    }

    pub(super) fn into_sha256(self) -> String {
        let digest = self.hasher.finalize();
        let mut hex = String::with_capacity(digest.len() * 2);
        for byte in digest {
            hex.push_str(&format!("{byte:02x}"));
        }
        hex
    }
}

impl<R: Read> Read for HashingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let read = self.inner.read(buf)?;
        self.hasher.update(&buf[..read]);
        Ok(read)
    }
}

/// Hash a file with a fixed-size buffer. Artifact identity must never require a
/// second allocation the size of the artifact being identified.
pub(super) fn sha256_file(path: &Path) -> CliResult<String> {
    let mut reader = BufReader::new(File::open(path)?);
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        hex.push_str(&format!("{byte:02x}"));
    }
    Ok(hex)
}

/// Atomic JSON write followed by a parent-directory fsync, so the rename that
/// publishes the file is itself durable before any later step (marker removal,
/// process exit) can be observed. Use for commit points whose ordering against
/// other filesystem state carries crash-recovery meaning (the search manifest
/// and the rebuild-required marker), not for bulk slot sidecars.
pub(super) fn write_json_atomic_durable<T: Serialize>(path: &Path, value: &T) -> CliResult {
    write_json_atomic(path, value)?;
    let parent = path.parent().ok_or_else(|| {
        stale(format!(
            "durable write path {} has no parent directory",
            path.display()
        ))
    })?;
    sync_dir(parent)
}

/// Remove `path` and fsync its parent directory so the removal is durable in
/// order. Missing file is an error at this layer — callers decide when absence
/// is legal and must check first.
pub(super) fn remove_file_durable(path: &Path) -> CliResult {
    fs::remove_file(path)?;
    let parent = path.parent().ok_or_else(|| {
        stale(format!(
            "durable remove path {} has no parent directory",
            path.display()
        ))
    })?;
    sync_dir(parent)
}

#[cfg(unix)]
fn sync_dir(dir: &Path) -> CliResult {
    let handle = File::open(dir)?;
    handle.sync_all()?;
    Ok(())
}

#[cfg(windows)]
fn sync_dir(dir: &Path) -> CliResult {
    use std::fs::OpenOptions;
    use std::os::windows::fs::OpenOptionsExt;

    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS;

    let handle = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(dir)?;
    handle.sync_all()?;
    Ok(())
}

pub(super) fn rel(root: &Path, path: &Path) -> CliResult<String> {
    let relative = path
        .strip_prefix(root)
        .map_err(|err| CliError::usage(format!("index path is outside vault root: {err}")))?;
    Ok(relative.to_string_lossy().replace('\\', "/"))
}

pub(super) fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

pub(super) fn stale(message: impl Into<String>) -> CliError {
    CalyxError::stale_derived(message).into()
}
