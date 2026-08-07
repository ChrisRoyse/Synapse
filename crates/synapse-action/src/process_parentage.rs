//! Exact, PID-reuse-guarded process parentage capture (#2089).
//!
//! Every process-observation row Synapse writes names the process it observed
//! by pid. A pid alone cannot carry a parent/child edge: Windows stores the
//! creating process id in the child's `EPROCESS` for posterity, but it does not
//! keep that pid reserved. Once the real parent exits, its pid is free for
//! reuse, and the child keeps pointing at a number that may now belong to an
//! unrelated process. Raymond Chen's writeup of exactly this
//! (`devblogs.microsoft.com/oldnewthing/20150403-00`) and the process-tree
//! construction it recommends
//! (`trainsec.net/library/windows-internals/building-a-process-tree/`) both
//! land on the same mitigation, which is what this module implements:
//!
//! 1. read the parent pid the kernel recorded for the child, and
//! 2. read the **creation time** of both the child and that parent pid's
//!    current occupant, and
//! 3. only believe the edge when the claimed parent was created **at or before**
//!    the child. A "parent" created after its child is proof of pid reuse, not
//!    a parent.
//!
//! The one source that supplies all three facts in a single, consistent
//! observation is `NtQuerySystemInformation(SystemProcessInformation)`. The
//! obvious alternative — `OpenProcess` + `GetProcessTimes` per pid — fails with
//! access-denied on protected processes, which is precisely the case where a
//! silent `None` would be indistinguishable from "no parent". The snapshot API
//! reports creation times for entries this process could never open, so the
//! unavailable cases that remain are real and are recorded with a reason.
//!
//! What this module deliberately does **not** claim: the recorded parent is the
//! process named by `InheritedFromUniqueProcessId`, which a creator may set to
//! an arbitrary process via `PROC_THREAD_ATTRIBUTE_PARENT_PROCESS`. That is the
//! parent for inheritance and job accounting — the relationship the process
//! graph is about — and it is not a claim about which process called
//! `CreateProcess`.
//!
//! Capture never fails into silence. Every outcome is a [`ProcessParentage`]
//! with a closed-vocabulary [`ParentageState`], and only
//! [`ParentageState::ParentVerified`] is allowed to produce a graph edge.

use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Schema version of the `parentage` object embedded in observation rows.
pub const PROCESS_PARENTAGE_SCHEMA_VERSION: u32 = 1;

/// Why a captured parentage record does or does not support a parent/child edge.
///
/// This vocabulary is closed and is persisted verbatim into observation rows;
/// consumers match on it and must reject anything they do not recognise rather
/// than guess.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParentageState {
    /// The kernel named a parent pid, that pid is currently occupied by a
    /// process created at or before the child, and both creation times were
    /// read from the same snapshot. This is the only edge-bearing state.
    ParentVerified,
    /// The kernel named a parent pid whose current occupant was created *after*
    /// the child. The real parent has exited and the pid was recycled; an edge
    /// here would be a fabrication.
    ParentPidRecycled,
    /// The kernel named a parent pid with no live process-table entry, or an
    /// entry whose creation time could not be read. The true parent is gone and
    /// the pid may or may not already have been reused — unknowable, so no edge.
    ParentIdentityUnavailable,
    /// The kernel recorded no parent for this process (a root, e.g. `System`).
    NoParentRecorded,
    /// The observed pid was not in the process table at capture time; the
    /// process exited before it could be observed.
    ChildAbsent,
    /// The process-table snapshot itself failed. Carries the exact failure text.
    SnapshotUnavailable,
    /// This build has no parentage observer for the host platform.
    PlatformUnsupported,
}

impl ParentageState {
    /// Stable wire spelling, identical to the serde representation.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ParentVerified => "parent_verified",
            Self::ParentPidRecycled => "parent_pid_recycled",
            Self::ParentIdentityUnavailable => "parent_identity_unavailable",
            Self::NoParentRecorded => "no_parent_recorded",
            Self::ChildAbsent => "child_absent",
            Self::SnapshotUnavailable => "snapshot_unavailable",
            Self::PlatformUnsupported => "platform_unsupported",
        }
    }
}

/// One exact parentage observation for one pid.
///
/// Every optional field carries `#[serde(default)]` so rows written before this
/// schema existed, and rows written by a future schema that drops a field, both
/// still decode.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProcessParentage {
    pub schema_version: u32,
    pub state: ParentageState,
    /// The pid this record is about.
    pub child_pid: u32,
    /// Creation FILETIME of the observed process, in 100 ns ticks since 1601.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub child_start_time_100ns: Option<u64>,
    /// The parent pid the kernel recorded for the child. Present whenever the
    /// kernel named one, **including** the recycled and unavailable states — the
    /// number is real evidence even when it must not become an edge.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_pid_observed: Option<u32>,
    /// Creation FILETIME of whatever process currently occupies
    /// `parent_pid_observed`. This is the pid-reuse guard.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_start_time_100ns: Option<u64>,
    /// Image name of the current occupant of `parent_pid_observed`, for human
    /// evidence. Never load-bearing for the edge decision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_image_name: Option<String>,
    /// True when the verified parent is the observing process itself. This is
    /// the strongest case available: the observer is alive throughout the
    /// capture, so its own pid cannot have been recycled underneath it.
    pub parent_is_observer: bool,
    /// The process that performed the observation.
    pub observer_pid: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observer_start_time_100ns: Option<u64>,
    /// How the start-time values were obtained.
    pub start_time_source: String,
    /// How the parentage itself was obtained.
    pub source: String,
    pub observed_at_unix_ms: u64,
    /// Exact, human-readable reason for every non-verified state. Never `None`
    /// when `state != ParentVerified`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unavailable_reason: Option<String>,
}

impl ProcessParentage {
    /// The parent pid this record is allowed to contribute as a graph edge.
    ///
    /// Returns `None` unless the state is [`ParentageState::ParentVerified`],
    /// both creation times are present, the parent was created at or before the
    /// child, and the pair is non-degenerate. A caller that wants the raw
    /// kernel claim regardless of trust reads `parent_pid_observed`.
    #[must_use]
    pub fn edge_parent_pid(&self) -> Option<u32> {
        if self.state != ParentageState::ParentVerified {
            return None;
        }
        let parent_pid = self.parent_pid_observed?;
        let parent_start = self.parent_start_time_100ns?;
        let child_start = self.child_start_time_100ns?;
        if parent_start > child_start {
            return None;
        }
        if parent_pid == 0 || self.child_pid == 0 || parent_pid == self.child_pid {
            return None;
        }
        Some(parent_pid)
    }

    fn unavailable(
        child_pid: u32,
        state: ParentageState,
        reason: String,
        observed_at_unix_ms: u64,
    ) -> Self {
        Self {
            schema_version: PROCESS_PARENTAGE_SCHEMA_VERSION,
            state,
            child_pid,
            child_start_time_100ns: None,
            parent_pid_observed: None,
            parent_start_time_100ns: None,
            parent_image_name: None,
            parent_is_observer: false,
            observer_pid: std::process::id(),
            observer_start_time_100ns: None,
            start_time_source: START_TIME_SOURCE.to_owned(),
            source: PARENTAGE_SOURCE.to_owned(),
            observed_at_unix_ms,
            unavailable_reason: Some(reason),
        }
    }
}

#[cfg(windows)]
const START_TIME_SOURCE: &str = "windows_ntquerysysteminformation_createtime_filetime_100ns";
#[cfg(not(windows))]
const START_TIME_SOURCE: &str = "unavailable_on_this_platform";

#[cfg(windows)]
const PARENTAGE_SOURCE: &str = "windows_ntquerysysteminformation_system_process_information";
#[cfg(not(windows))]
const PARENTAGE_SOURCE: &str = "unavailable_on_this_platform";

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

/// One process-table entry from a single kernel snapshot.
#[derive(Clone, Debug)]
pub struct SystemProcessEntry {
    pub pid: u32,
    /// `InheritedFromUniqueProcessId`. Zero means the kernel recorded no parent.
    pub parent_pid: u32,
    /// Creation FILETIME in 100 ns ticks since 1601.
    pub creation_time_100ns: u64,
    pub image_name: Option<String>,
}

/// Capture the exact parentage of `child_pid`, with the pid-reuse guard applied.
///
/// This function never returns an error: an observation that cannot be made is
/// an observation that says so. Callers persist the record as-is.
#[must_use]
pub fn capture_process_parentage(child_pid: u32) -> ProcessParentage {
    let observed_at_unix_ms = now_unix_ms();
    #[cfg(not(windows))]
    {
        ProcessParentage::unavailable(
            child_pid,
            ParentageState::PlatformUnsupported,
            "process parentage capture is implemented only on Windows; this build cannot observe \
             parent pids or their creation times"
                .to_owned(),
            observed_at_unix_ms,
        )
    }
    #[cfg(windows)]
    {
        capture_process_parentage_windows(child_pid, observed_at_unix_ms)
    }
}

#[cfg(windows)]
fn capture_process_parentage_windows(child_pid: u32, observed_at_unix_ms: u64) -> ProcessParentage {
    let snapshot = match system_process_snapshot() {
        Ok(snapshot) => snapshot,
        Err(error) => {
            return ProcessParentage::unavailable(
                child_pid,
                ParentageState::SnapshotUnavailable,
                format!("kernel process-table snapshot failed: {error}"),
                observed_at_unix_ms,
            );
        }
    };
    let observer_pid = std::process::id();
    let observer_start_time_100ns = snapshot
        .get(&observer_pid)
        .map(|entry| entry.creation_time_100ns);
    let Some(child) = snapshot.get(&child_pid) else {
        let mut record = ProcessParentage::unavailable(
            child_pid,
            ParentageState::ChildAbsent,
            format!(
                "pid {child_pid} had no process-table entry at capture time; it exited before its \
                 parentage could be observed"
            ),
            observed_at_unix_ms,
        );
        record.observer_start_time_100ns = observer_start_time_100ns;
        return record;
    };
    let base = ProcessParentage {
        schema_version: PROCESS_PARENTAGE_SCHEMA_VERSION,
        state: ParentageState::ParentVerified,
        child_pid,
        child_start_time_100ns: Some(child.creation_time_100ns),
        parent_pid_observed: None,
        parent_start_time_100ns: None,
        parent_image_name: None,
        parent_is_observer: false,
        observer_pid,
        observer_start_time_100ns,
        start_time_source: START_TIME_SOURCE.to_owned(),
        source: PARENTAGE_SOURCE.to_owned(),
        observed_at_unix_ms,
        unavailable_reason: None,
    };
    if child.parent_pid == 0 {
        return ProcessParentage {
            state: ParentageState::NoParentRecorded,
            unavailable_reason: Some(format!(
                "the kernel recorded no parent for pid {child_pid}; it is a root process"
            )),
            ..base
        };
    }
    let parent_pid = child.parent_pid;
    let Some(parent) = snapshot.get(&parent_pid) else {
        return ProcessParentage {
            state: ParentageState::ParentIdentityUnavailable,
            parent_pid_observed: Some(parent_pid),
            unavailable_reason: Some(format!(
                "the kernel recorded parent pid {parent_pid} for pid {child_pid}, but that pid had \
                 no process-table entry at capture time; the real parent exited and the pid may \
                 since be recycled, so the edge is unprovable"
            )),
            ..base
        };
    };
    apply_pid_reuse_guard(child.creation_time_100ns, parent, base)
}

/// The pid-reuse guard itself: a claimed parent created after its child is not
/// a parent, it is the next occupant of a recycled pid.
#[cfg(windows)]
fn apply_pid_reuse_guard(
    child_start_time_100ns: u64,
    parent: &SystemProcessEntry,
    base: ProcessParentage,
) -> ProcessParentage {
    let parent_pid = parent.pid;
    let child_pid = base.child_pid;
    if parent.creation_time_100ns == 0 {
        return ProcessParentage {
            state: ParentageState::ParentIdentityUnavailable,
            parent_pid_observed: Some(parent_pid),
            parent_image_name: parent.image_name.clone(),
            unavailable_reason: Some(format!(
                "parent pid {parent_pid} for pid {child_pid} exposed no creation time, so the \
                 pid-reuse guard cannot be evaluated"
            )),
            ..base
        };
    }
    if parent.creation_time_100ns > child_start_time_100ns {
        return ProcessParentage {
            state: ParentageState::ParentPidRecycled,
            parent_pid_observed: Some(parent_pid),
            parent_start_time_100ns: Some(parent.creation_time_100ns),
            parent_image_name: parent.image_name.clone(),
            unavailable_reason: Some(format!(
                "pid {parent_pid} was created at {} which is after pid {child_pid} at \
                 {child_start_time_100ns}; the process now holding that pid cannot be the parent, \
                 so the pid was recycled",
                parent.creation_time_100ns
            )),
            ..base
        };
    }
    let parent_is_observer = parent_pid == base.observer_pid;
    ProcessParentage {
        parent_pid_observed: Some(parent_pid),
        parent_start_time_100ns: Some(parent.creation_time_100ns),
        parent_image_name: parent.image_name.clone(),
        parent_is_observer,
        ..base
    }
}

/// Snapshot the whole process table in one kernel call.
///
/// The map is keyed by pid; the idle process (pid 0) is excluded because it is
/// not a process any observation can be about.
///
/// # Errors
///
/// Returns the exact failure text when the kernel call, its buffer growth, or
/// the entry walk fails.
#[cfg(windows)]
pub fn system_process_snapshot()
-> Result<std::collections::BTreeMap<u32, SystemProcessEntry>, String> {
    use std::collections::BTreeMap;

    use windows::Win32::System::WindowsProgramming::SYSTEM_PROCESS_INFORMATION;

    let (buffer, used_bytes) = query_system_process_information()?;
    let mut entries = BTreeMap::<u32, SystemProcessEntry>::new();
    let mut offset = 0_usize;
    loop {
        let entry_end = offset
            .checked_add(std::mem::size_of::<SYSTEM_PROCESS_INFORMATION>())
            .filter(|end| *end <= used_bytes);
        if entry_end.is_none() {
            return Err(format!(
                "process snapshot entry at offset {offset} exceeds returned length {used_bytes}"
            ));
        }
        // SAFETY: bounds were checked above. The entry bytes are copied into an
        // aligned local because the Windows byte stream's alignment is an
        // external ABI fact, not a guarantee carried by the `u8` offset pointer.
        let process = unsafe {
            let process_bytes = buffer.as_ptr().cast::<u8>().add(offset);
            let mut process = std::mem::MaybeUninit::<SYSTEM_PROCESS_INFORMATION>::uninit();
            std::ptr::copy_nonoverlapping(
                process_bytes,
                process.as_mut_ptr().cast::<u8>(),
                std::mem::size_of::<SYSTEM_PROCESS_INFORMATION>(),
            );
            process.assume_init()
        };
        if let Some(entry) = decode_process_entry(&process)? {
            entries.insert(entry.pid, entry);
        }

        let next = usize::try_from(process.NextEntryOffset)
            .map_err(|_error| "process snapshot next-entry offset exceeds usize")?;
        if next == 0 {
            break;
        }
        if next < std::mem::size_of::<SYSTEM_PROCESS_INFORMATION>() {
            return Err(format!(
                "process snapshot returned invalid next-entry offset {next}"
            ));
        }
        offset = offset
            .checked_add(next)
            .ok_or_else(|| "process snapshot offset overflow".to_owned())?;
    }
    Ok(entries)
}

/// Call `NtQuerySystemInformation(SystemProcessInformation)`, growing the buffer
/// until the kernel accepts it. Returns the buffer and the number of bytes the
/// kernel actually wrote.
#[cfg(windows)]
fn query_system_process_information() -> Result<(Vec<usize>, usize), String> {
    use windows::Wdk::System::SystemInformation::{
        NtQuerySystemInformation, SystemProcessInformation,
    };

    const STATUS_INFO_LENGTH_MISMATCH: i32 = 0xC000_0004_u32.cast_signed();
    const INITIAL_BUFFER_BYTES: usize = 64 * 1024;

    let word_size = std::mem::size_of::<usize>();
    let mut buffer = vec![0_usize; INITIAL_BUFFER_BYTES.div_ceil(word_size)];
    loop {
        let buffer_bytes = buffer
            .len()
            .checked_mul(word_size)
            .ok_or_else(|| "process snapshot buffer size overflow".to_owned())?;
        let buffer_bytes_u32 = u32::try_from(buffer_bytes)
            .map_err(|_error| "process snapshot buffer exceeds the Windows u32 length contract")?;
        let mut returned_bytes = 0_u32;
        // SAFETY: `buffer` is pointer-aligned storage with exactly the supplied
        // writable byte length. NtQuerySystemInformation reports the required or
        // written length through `returned_bytes`.
        let status = unsafe {
            NtQuerySystemInformation(
                SystemProcessInformation,
                buffer.as_mut_ptr().cast(),
                buffer_bytes_u32,
                &raw mut returned_bytes,
            )
        };
        if status.0 == STATUS_INFO_LENGTH_MISMATCH {
            let required = usize::try_from(returned_bytes)
                .map_err(|_error| "process snapshot required length exceeds usize")?;
            let grown = required
                .max(buffer_bytes.saturating_mul(2))
                .checked_add(INITIAL_BUFFER_BYTES)
                .ok_or_else(|| "process snapshot growth overflow".to_owned())?;
            buffer.resize(grown.div_ceil(word_size), 0);
            continue;
        }
        if status.0 < 0 {
            return Err(format!(
                "NtQuerySystemInformation(SystemProcessInformation) failed: ntstatus=0x{:08X}",
                status.0.cast_unsigned()
            ));
        }
        let returned_bytes = usize::try_from(returned_bytes)
            .map_err(|_error| "process snapshot returned length exceeds usize")?;
        let used_bytes = if returned_bytes == 0 {
            buffer_bytes
        } else {
            returned_bytes.min(buffer_bytes)
        };
        return Ok((buffer, used_bytes));
    }
}

/// Decode one snapshot entry. Returns `Ok(None)` for the idle process (pid 0),
/// which is not a process any observation can be about.
#[cfg(windows)]
fn decode_process_entry(
    process: &windows::Win32::System::WindowsProgramming::SYSTEM_PROCESS_INFORMATION,
) -> Result<Option<SystemProcessEntry>, String> {
    // In the documented SYSTEM_PROCESS_INFORMATION layout, `Reserved1` contains
    // WorkingSetPrivateSize, HardFaultCount, NumberOfThreadsHighWatermark,
    // CycleTime, CreateTime, UserTime and KernelTime. CreateTime begins 24 bytes
    // into this exact 48-byte field.
    const CREATION_TIME_OFFSET_IN_RESERVED1: usize = 24;
    // A Windows image name is a path component; anything longer than this is a
    // corrupt read, not a name.
    const MAX_IMAGE_NAME_BYTES: usize = 2 * 32_768;

    let pid_value = process.UniqueProcessId.0 as usize;
    if pid_value == 0 {
        return Ok(None);
    }
    let pid = u32::try_from(pid_value)
        .map_err(|_error| format!("process snapshot pid {pid_value} exceeds u32"))?;
    let creation_bytes: [u8; 8] = process.Reserved1
        [CREATION_TIME_OFFSET_IN_RESERVED1..CREATION_TIME_OFFSET_IN_RESERVED1 + 8]
        .try_into()
        .map_err(|_error| "process snapshot creation-time slice was not 8 bytes")?;
    let creation_time = i64::from_ne_bytes(creation_bytes);
    // A negative FILETIME is not a real creation time; report it as absent so
    // the pid-reuse guard refuses the edge instead of comparing garbage.
    let creation_time_100ns = u64::try_from(creation_time).unwrap_or(0);
    let parent_pid_value = process.Reserved2 as usize;
    let parent_pid = u32::try_from(parent_pid_value)
        .map_err(|_error| format!("process snapshot parent pid {parent_pid_value} exceeds u32"))?;
    let name_length = usize::from(process.ImageName.Length);
    let image_name = if process.ImageName.Buffer.is_null()
        || name_length == 0
        || name_length % 2 != 0
        || name_length > MAX_IMAGE_NAME_BYTES
    {
        None
    } else {
        // SAFETY: the kernel returns `ImageName.Buffer` pointing into the buffer
        // it just filled, valid for `Length` bytes. The length is bounded and
        // even-checked above, and the units are copied into an owned `String`
        // before the buffer is touched again.
        let units =
            unsafe { std::slice::from_raw_parts(process.ImageName.Buffer.0, name_length / 2) };
        Some(String::from_utf16_lossy(units))
    };
    Ok(Some(SystemProcessEntry {
        pid,
        parent_pid,
        creation_time_100ns,
        image_name,
    }))
}
