//! Disk-pressure guard for fail-closed hotpool write admission.

use calyx_core::{CalyxError, Clock, Result, Ts};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Sender;

pub const CALYX_IO_ERROR: &str = "CALYX_IO_ERROR";
pub const DEFAULT_HIGH_WATER_RATIO: f64 = 0.85;

const IO_REMEDIATION: &str = "inspect hotpool path, permissions, and statvfs support; reject writes until disk state is readable";

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DiskSample {
    pub blocks: u64,
    pub blocks_available: u64,
}

impl DiskSample {
    pub fn used_ratio(self) -> f64 {
        if self.blocks == 0 {
            return 0.0;
        }
        let available = self.blocks_available.min(self.blocks) as f64 / self.blocks as f64;
        (1.0 - available).clamp(0.0, 1.0)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum DiskStatus {
    Ok {
        hotpool_path: PathBuf,
        used_ratio: f64,
        blocks_available: u64,
        blocks_total: u64,
        checked_at: Ts,
    },
}

pub trait DiskSpaceProbe: Send + Sync {
    fn sample(&self, path: &Path) -> Result<DiskSample>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct OsDiskSpaceProbe;

impl DiskSpaceProbe for OsDiskSpaceProbe {
    fn sample(&self, path: &Path) -> Result<DiskSample> {
        os_disk_sample(path)
    }
}

#[derive(Clone)]
pub struct DiskPressureGuard {
    pub hotpool_path: PathBuf,
    pub high_water_ratio: f64,
    pub clock: Arc<dyn Clock>,
    probe: Arc<dyn DiskSpaceProbe>,
    spill_trigger: Option<SpillTrigger>,
}

impl fmt::Debug for DiskPressureGuard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DiskPressureGuard")
            .field("hotpool_path", &self.hotpool_path)
            .field("high_water_ratio", &self.high_water_ratio)
            .field("spill_trigger", &self.spill_trigger)
            .finish_non_exhaustive()
    }
}

impl DiskPressureGuard {
    pub fn new(
        hotpool_path: impl Into<PathBuf>,
        high_water_ratio: f64,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self::with_probe(
            hotpool_path,
            high_water_ratio,
            clock,
            Arc::new(OsDiskSpaceProbe),
        )
    }

    pub fn with_probe(
        hotpool_path: impl Into<PathBuf>,
        high_water_ratio: f64,
        clock: Arc<dyn Clock>,
        probe: Arc<dyn DiskSpaceProbe>,
    ) -> Self {
        Self {
            hotpool_path: hotpool_path.into(),
            high_water_ratio,
            clock,
            probe,
            spill_trigger: None,
        }
    }

    pub fn with_spill_trigger(mut self, spill_trigger: SpillTrigger) -> Self {
        self.spill_trigger = Some(spill_trigger);
        self
    }

    pub fn check(&self) -> Result<DiskStatus> {
        if !(0.0..=1.0).contains(&self.high_water_ratio) {
            return Err(CalyxError::disk_pressure(format!(
                "invalid high_water_ratio={:.6} for hotpool {}",
                self.high_water_ratio,
                self.hotpool_path.display()
            )));
        }
        let sample = self.probe.sample(&self.hotpool_path)?;
        let used_ratio = sample.used_ratio();
        let checked_at = self.clock.now();
        if used_ratio >= self.high_water_ratio {
            return Err(CalyxError::disk_pressure(format!(
                "hotpool {} used_ratio={used_ratio:.6} high_water_ratio={:.6} blocks_available={} blocks_total={} checked_at={checked_at}",
                self.hotpool_path.display(),
                self.high_water_ratio,
                sample.blocks_available,
                sample.blocks
            )));
        }
        Ok(DiskStatus::Ok {
            hotpool_path: self.hotpool_path.clone(),
            used_ratio,
            blocks_available: sample.blocks_available,
            blocks_total: sample.blocks,
            checked_at,
        })
    }

    pub fn is_under_pressure(&self) -> bool {
        self.check().is_err()
    }

    pub fn request_spill(&self) {
        if let Some(trigger) = &self.spill_trigger {
            trigger.request_spill();
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpillRequest {
    pub hotpool_path: PathBuf,
    pub requested_at: Ts,
}

#[derive(Clone)]
pub struct SpillTrigger {
    sender: Sender<SpillRequest>,
    hotpool_path: PathBuf,
    clock: Arc<dyn Clock>,
}

impl fmt::Debug for SpillTrigger {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SpillTrigger")
            .field("hotpool_path", &self.hotpool_path)
            .finish_non_exhaustive()
    }
}

impl SpillTrigger {
    pub fn new(
        hotpool_path: impl Into<PathBuf>,
        sender: Sender<SpillRequest>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            sender,
            hotpool_path: hotpool_path.into(),
            clock,
        }
    }

    pub fn request_spill(&self) {
        let request = SpillRequest {
            hotpool_path: self.hotpool_path.clone(),
            requested_at: self.clock.now(),
        };
        match self.sender.send(request) {
            Ok(()) => tracing::warn!(
                hotpool_path = %self.hotpool_path.display(),
                "disk pressure spill requested"
            ),
            Err(error) => tracing::warn!(
                hotpool_path = %self.hotpool_path.display(),
                error = %error,
                "disk pressure spill request dropped"
            ),
        }
    }
}

#[derive(Debug)]
pub struct TempFile {
    path: PathBuf,
    file: File,
    keep: bool,
}

impl TempFile {
    pub fn in_dataset(destination_dir: &Path) -> Result<Self> {
        fs::create_dir_all(destination_dir).map_err(|error| {
            io_error(format!(
                "create destination dataset {}: {error}",
                destination_dir.display()
            ))
        })?;
        for _ in 0..128 {
            let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
            let path = destination_dir.join(format!(".calyx-tmp-{}-{id}.tmp", std::process::id()));
            match OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(file) => {
                    return Ok(Self {
                        path,
                        file,
                        keep: false,
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(io_error(format!(
                        "create temp file in dataset {}: {error}",
                        destination_dir.display()
                    )));
                }
            }
        }
        Err(io_error(format!(
            "create temp file in dataset {}: exhausted unique names",
            destination_dir.display()
        )))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn file_mut(&mut self) -> &mut File {
        &mut self.file
    }

    pub fn persist(mut self) -> Result<PathBuf> {
        self.file.sync_all().map_err(|error| {
            io_error(format!("sync temp file {}: {error}", self.path.display()))
        })?;
        self.keep = true;
        Ok(self.path.clone())
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        if !self.keep {
            let _ = fs::remove_file(&self.path);
        }
    }
}

#[cfg(unix)]
fn os_disk_sample(path: &Path) -> Result<DiskSample> {
    let stat = nix::sys::statvfs::statvfs(path)
        .map_err(|error| io_error(format!("statvfs {}: {error}", path.display())))?;
    Ok(DiskSample {
        blocks: stat.blocks(),
        blocks_available: stat.blocks_available(),
    })
}

#[cfg(windows)]
fn os_disk_sample(path: &Path) -> Result<DiskSample> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;

    let mut wide = path.as_os_str().encode_wide().collect::<Vec<_>>();
    wide.push(0);
    let mut available = 0_u64;
    let mut total = 0_u64;
    let mut total_free = 0_u64;
    // SAFETY: `wide` is a live NUL-terminated UTF-16 path and every output
    // points to a writable u64 for the complete call.
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &raw mut available,
            &raw mut total,
            &raw mut total_free,
        )
    };
    if ok == 0 {
        return Err(io_error(format!(
            "GetDiskFreeSpaceExW {}: {}",
            path.display(),
            std::io::Error::last_os_error()
        )));
    }
    Ok(DiskSample {
        // The fields are unitless to callers and only used as a ratio. Bytes
        // preserve exactness and avoid a second filesystem query for cluster
        // size.
        blocks: total,
        blocks_available: available,
    })
}

#[cfg(not(any(unix, windows)))]
fn os_disk_sample(path: &Path) -> Result<DiskSample> {
    Err(io_error(format!(
        "disk-space probing unsupported for {}",
        path.display()
    )))
}

fn io_error(message: impl Into<String>) -> CalyxError {
    CalyxError {
        code: CALYX_IO_ERROR,
        message: message.into(),
        remediation: IO_REMEDIATION,
    }
}
