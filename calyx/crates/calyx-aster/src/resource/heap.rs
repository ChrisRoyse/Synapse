//! Process resident-set probe over the operating system's physical source.

use calyx_core::{CalyxError, Result};

/// Stable code for resource probes that cannot run on this host.
pub const CALYX_RESOURCE_PROBE_UNAVAILABLE: &str = "CALYX_RESOURCE_PROBE_UNAVAILABLE";

pub(crate) fn probe_unavailable(message: impl Into<String>) -> CalyxError {
    CalyxError {
        code: CALYX_RESOURCE_PROBE_UNAVAILABLE,
        message: message.into(),
        remediation: "run resource_status on a Linux host with /proc mounted",
    }
}

/// Reads the resident set size of this process in bytes.
///
/// On Linux the source of truth is the kernel `VmRSS` line in
/// `/proc/self/status` (`proc_pid_status(5)`). On Windows it is the current
/// process `WorkingSetSize` returned by `K32GetProcessMemoryInfo`. Unsupported
/// hosts and failed kernel calls fail closed with
/// `CALYX_RESOURCE_PROBE_UNAVAILABLE`.
///
/// # This is not the number to make a memory decision from on Windows
///
/// `WorkingSetSize` is the subset of the process's committed memory that is
/// currently *resident in physical RAM*. Windows trims working sets under system
/// pressure, so it falls without the process having freed anything: on the
/// daemon in #2122 it oscillated between 4,731 MB and 12,158 MB while the
/// process's actual committed private memory rose monotonically from 25,599 MB
/// to 28,206 MB. Reading working set there would have reported a healthy,
/// self-correcting sawtooth over a 1.07 GB/hour leak — wrong by 2-5x, and wrong
/// in the direction that suppresses the alarm.
///
/// Use [`process_private_bytes`] for anything that *decides*. This stays
/// because it is the honest answer to a different question (how much of us the
/// OS is currently keeping in RAM), and because the Prometheus series named
/// `calyx_heap_rss_bytes` has to keep meaning what it has always meant.
pub fn heap_rss_bytes() -> Result<u64> {
    #[cfg(target_os = "linux")]
    {
        let text = std::fs::read_to_string("/proc/self/status")
            .map_err(|error| probe_unavailable(format!("read /proc/self/status: {error}")))?;
        parse_vm_rss_bytes(&text)
    }
    #[cfg(target_os = "windows")]
    {
        windows_working_set_bytes()
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        Err(probe_unavailable(
            "heap RSS probe has no authoritative implementation for this operating system",
        ))
    }
}

#[cfg(target_os = "windows")]
fn windows_working_set_bytes() -> Result<u64> {
    use windows_sys::Win32::System::ProcessStatus::{
        K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    let mut counters = PROCESS_MEMORY_COUNTERS {
        cb: u32::try_from(std::mem::size_of::<PROCESS_MEMORY_COUNTERS>())
            .expect("PROCESS_MEMORY_COUNTERS size fits u32"),
        ..PROCESS_MEMORY_COUNTERS::default()
    };
    // SAFETY: GetCurrentProcess returns the documented pseudo-handle for this
    // process, and `counters` is a correctly sized, writable structure.
    let succeeded =
        unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, counters.cb) };
    if succeeded == 0 {
        return Err(probe_unavailable(format!(
            "K32GetProcessMemoryInfo(current process): {}",
            std::io::Error::last_os_error()
        )));
    }
    u64::try_from(counters.WorkingSetSize).map_err(|error| {
        probe_unavailable(format!(
            "convert Windows WorkingSetSize {} to u64: {error}",
            counters.WorkingSetSize
        ))
    })
}

/// Reads this process's **committed private** memory in bytes.
///
/// This is the number that grows when a process leaks and does not fall when
/// the OS reclaims pages, which makes it the only one of the two safe to key a
/// memory-pressure decision off.
///
/// * **Windows**: `PROCESS_MEMORY_COUNTERS_EX::PrivateUsage` — the commit
///   charge of this process's private (non-shared) pages, the same quantity
///   Task Manager calls "Commit size" and Performance Monitor calls
///   `Process\Private Bytes`. It counts committed pages whether or not they are
///   resident, so a working-set trim does not move it.
/// * **Linux**: the `RssAnon` line of `/proc/self/status` — resident anonymous
///   memory, i.e. the heap and stacks, excluding file-backed and shared
///   mappings. This is the closest kernel-authoritative analogue; Linux has no
///   per-process commit charge to read, and inventing one by summing
///   `/proc/self/smaps` would be a different measurement wearing this name.
///
/// The two are deliberately *not* claimed to be the same quantity. They are the
/// same *decision*: "how much memory has this process taken and not given back".
///
/// # Errors
///
/// Fails closed with `CALYX_RESOURCE_PROBE_UNAVAILABLE` on unsupported hosts and
/// on failed kernel reads. There is no fallback to working set — substituting
/// the metric that is wrong by 2-5x for the metric that is unavailable is how a
/// blind pressure decision gets made while looking healthy.
pub fn process_private_bytes() -> Result<u64> {
    #[cfg(target_os = "linux")]
    {
        let text = std::fs::read_to_string("/proc/self/status")
            .map_err(|error| probe_unavailable(format!("read /proc/self/status: {error}")))?;
        parse_status_kb_line(&text, "RssAnon:")
    }
    #[cfg(target_os = "windows")]
    {
        windows_private_bytes()
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        Err(probe_unavailable(
            "private-commit probe has no authoritative implementation for this operating system",
        ))
    }
}

#[cfg(target_os = "windows")]
fn windows_private_bytes() -> Result<u64> {
    use windows_sys::Win32::System::ProcessStatus::{
        K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    let mut counters = PROCESS_MEMORY_COUNTERS_EX {
        cb: u32::try_from(std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>())
            .expect("PROCESS_MEMORY_COUNTERS_EX size fits u32"),
        ..PROCESS_MEMORY_COUNTERS_EX::default()
    };
    let cb = counters.cb;
    // SAFETY: GetCurrentProcess returns the documented pseudo-handle for this
    // process. `PROCESS_MEMORY_COUNTERS_EX` is the documented extended form of
    // `PROCESS_MEMORY_COUNTERS` (same prefix layout, one extra trailing field);
    // passing it through the `PROCESS_MEMORY_COUNTERS` parameter with its own
    // `cb` is exactly how the Win32 documentation specifies retrieving
    // `PrivateUsage`, and the kernel writes at most `cb` bytes.
    let succeeded = unsafe {
        K32GetProcessMemoryInfo(
            GetCurrentProcess(),
            std::ptr::from_mut(&mut counters).cast::<PROCESS_MEMORY_COUNTERS>(),
            cb,
        )
    };
    if succeeded == 0 {
        return Err(probe_unavailable(format!(
            "K32GetProcessMemoryInfo(current process, EX): {}",
            std::io::Error::last_os_error()
        )));
    }
    u64::try_from(counters.PrivateUsage).map_err(|error| {
        probe_unavailable(format!(
            "convert Windows PrivateUsage {} to u64: {error}",
            counters.PrivateUsage
        ))
    })
}

/// Parses the `VmRSS:` line of a `/proc/<pid>/status` document into bytes.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn parse_vm_rss_bytes(status_text: &str) -> Result<u64> {
    parse_status_kb_line(status_text, "VmRSS:")
}

/// Parses one `kB`-valued line of a `/proc/<pid>/status` document into bytes.
///
/// Generalized from the `VmRSS:`-only parser because `RssAnon:` is read the same
/// way and copying the parser would have given the two probes two independent
/// unit checks to drift apart. The unit is verified rather than assumed: the
/// kernel documents kB, and a silent scale guess is the kind of error that
/// reports a leak as a rounding difference.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn parse_status_kb_line(status_text: &str, prefix: &str) -> Result<u64> {
    let name = prefix.trim_end_matches(':');
    for line in status_text.lines() {
        let Some(rest) = line.strip_prefix(prefix) else {
            continue;
        };
        let mut fields = rest.split_whitespace();
        let value = fields
            .next()
            .ok_or_else(|| probe_unavailable(format!("{name} line has no value field")))?;
        let unit = fields
            .next()
            .ok_or_else(|| probe_unavailable(format!("{name} line has no unit field")))?;
        if unit != "kB" {
            return Err(probe_unavailable(format!(
                "{name} unit {unit:?} is not kB; refusing to guess a scale"
            )));
        }
        let kib = value
            .parse::<u64>()
            .map_err(|error| probe_unavailable(format!("parse {name} value {value:?}: {error}")))?;
        return Ok(kib.saturating_mul(1024));
    }
    Err(probe_unavailable(format!(
        "{name} line not found in /proc/self/status"
    )))
}
