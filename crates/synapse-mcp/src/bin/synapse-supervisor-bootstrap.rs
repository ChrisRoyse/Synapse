//! Minimal native bootstrap for the installed Windows daemon supervisor.
//!
//! Task Scheduler starts this binary instead of PowerShell. The bootstrap binds
//! itself to the authoritative whole-owned-tree Job before it launches any
//! managed runtime. Its PowerShell child inherits that Job at process creation;
//! the bootstrap proves exact kernel membership while the child is suspended,
//! then resumes it and retains the Job handle for the supervisor lifetime.

#[cfg(not(windows))]
fn main() {
    eprintln!(
        "SYNAPSE_SUPERVISOR_BOOTSTRAP_UNSUPPORTED_PLATFORM: Windows Job Objects are required; remediation=run the installed supervisor bootstrap only on Windows"
    );
    std::process::exit(1);
}

#[cfg(windows)]
mod windows_bootstrap {
    use std::ffi::{OsStr, OsString, c_void};
    use std::fs::OpenOptions;
    use std::io::{self, Write};
    use std::mem::{ManuallyDrop, size_of, zeroed};
    use std::os::windows::ffi::OsStrExt;
    use std::path::{Path, PathBuf};
    use std::ptr::{null, null_mut};

    use synapse_core::{SYNAPSE_OWNED_TREE_CPU_RATE, SYNAPSE_OWNED_TREE_HARD_LIMIT_BYTES};

    type Handle = *mut c_void;
    type Bool = i32;

    const FALSE: Bool = 0;
    const CREATE_SUSPENDED: u32 = 0x0000_0004;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const ERROR_ALREADY_EXISTS: u32 = 183;
    const JOB_OBJECT_LIMIT_PROCESS_MEMORY: u32 = 0x0000_0100;
    const JOB_OBJECT_LIMIT_JOB_MEMORY: u32 = 0x0000_0200;
    const JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE: u32 = 0x0000_2000;
    const EXPECTED_JOB_LIMIT_FLAGS: u32 = JOB_OBJECT_LIMIT_PROCESS_MEMORY
        | JOB_OBJECT_LIMIT_JOB_MEMORY
        | JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    const JOB_OBJECT_CPU_RATE_CONTROL_ENABLE: u32 = 0x0000_0001;
    const JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP: u32 = 0x0000_0004;
    const EXPECTED_CPU_RATE_FLAGS: u32 =
        JOB_OBJECT_CPU_RATE_CONTROL_ENABLE | JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP;
    const JOB_OBJECT_EXTENDED_LIMIT_INFORMATION: i32 = 9;
    const JOB_OBJECT_CPU_RATE_CONTROL_INFORMATION: i32 = 15;
    const JOB_OBJECT_MEMORY_USAGE_INFORMATION: i32 = 28;
    const WAIT_OBJECT_0: u32 = 0;
    const WAIT_TIMEOUT: u32 = 0x0000_0102;
    const WAIT_FAILED: u32 = 0xffff_ffff;
    const INFINITE: u32 = 0xffff_ffff;
    const CHILD_TERMINATION_WAIT_MS: u32 = 30_000;
    const SECURITY_DESCRIPTOR_REVISION: u32 = 1;
    const TOTAL_COMMITTED_PRIVATE_POLICY_CEILING_BYTES: u64 = 1_000_000_000;
    const JOB_SECURITY_DESCRIPTOR_SDDL: &str = "D:P(A;;GA;;;SY)(A;;0x00100004;;;OW)";

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct IoCounters {
        read_operation_count: u64,
        write_operation_count: u64,
        other_operation_count: u64,
        read_transfer_count: u64,
        write_transfer_count: u64,
        other_transfer_count: u64,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct JobObjectBasicLimitInformation {
        per_process_user_time_limit: i64,
        per_job_user_time_limit: i64,
        limit_flags: u32,
        minimum_working_set_size: usize,
        maximum_working_set_size: usize,
        active_process_limit: u32,
        affinity: usize,
        priority_class: u32,
        scheduling_class: u32,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct JobObjectExtendedLimitInformation {
        basic_limit_information: JobObjectBasicLimitInformation,
        io_info: IoCounters,
        process_memory_limit: usize,
        job_memory_limit: usize,
        peak_process_memory_used: usize,
        peak_job_memory_used: usize,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct JobObjectCpuRateControlInformation {
        control_flags: u32,
        cpu_rate: u32,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct JobObjectMemoryUsageInformation {
        job_memory: u64,
        peak_job_memory_used: u64,
    }

    #[repr(C)]
    struct StartupInfoW {
        cb: u32,
        lp_reserved: *mut u16,
        lp_desktop: *mut u16,
        lp_title: *mut u16,
        dw_x: u32,
        dw_y: u32,
        dw_x_size: u32,
        dw_y_size: u32,
        dw_x_count_chars: u32,
        dw_y_count_chars: u32,
        dw_fill_attribute: u32,
        dw_flags: u32,
        w_show_window: u16,
        cb_reserved_2: u16,
        lp_reserved_2: *mut u8,
        std_input: Handle,
        std_output: Handle,
        std_error: Handle,
    }

    #[repr(C)]
    struct SecurityAttributes {
        length: u32,
        security_descriptor: *mut c_void,
        inherit_handle: Bool,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct ProcessMemoryCountersEx {
        cb: u32,
        page_fault_count: u32,
        peak_working_set_size: usize,
        working_set_size: usize,
        quota_peak_paged_pool_usage: usize,
        quota_paged_pool_usage: usize,
        quota_peak_non_paged_pool_usage: usize,
        quota_non_paged_pool_usage: usize,
        pagefile_usage: usize,
        peak_pagefile_usage: usize,
        private_usage: usize,
    }

    #[repr(C)]
    struct ProcessInformation {
        process: Handle,
        thread: Handle,
        process_id: u32,
        thread_id: u32,
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentProcess() -> Handle;
        fn GetCurrentProcessId() -> u32;
        fn CreateJobObjectW(attributes: *const SecurityAttributes, name: *const u16) -> Handle;
        fn GetLastError() -> u32;
        fn SetInformationJobObject(
            job: Handle,
            information_class: i32,
            information: *const c_void,
            information_length: u32,
        ) -> Bool;
        fn QueryInformationJobObject(
            job: Handle,
            information_class: i32,
            information: *mut c_void,
            information_length: u32,
            return_length: *mut u32,
        ) -> Bool;
        fn AssignProcessToJobObject(job: Handle, process: Handle) -> Bool;
        fn IsProcessInJob(process: Handle, job: Handle, result: *mut Bool) -> Bool;
        fn CreateProcessW(
            application_name: *const u16,
            command_line: *mut u16,
            process_attributes: *const c_void,
            thread_attributes: *const c_void,
            inherit_handles: Bool,
            creation_flags: u32,
            environment: *const c_void,
            current_directory: *const u16,
            startup_info: *mut StartupInfoW,
            process_information: *mut ProcessInformation,
        ) -> Bool;
        fn ResumeThread(thread: Handle) -> u32;
        fn WaitForSingleObject(handle: Handle, milliseconds: u32) -> u32;
        fn GetExitCodeProcess(process: Handle, exit_code: *mut u32) -> Bool;
        fn TerminateProcess(process: Handle, exit_code: u32) -> Bool;
        fn CloseHandle(handle: Handle) -> Bool;
        fn LocalFree(memory: *mut c_void) -> *mut c_void;
    }

    #[link(name = "advapi32")]
    unsafe extern "system" {
        fn ConvertStringSecurityDescriptorToSecurityDescriptorW(
            string_security_descriptor: *const u16,
            string_sd_revision: u32,
            security_descriptor: *mut *mut c_void,
            security_descriptor_size: *mut u32,
        ) -> Bool;
    }

    #[link(name = "psapi")]
    unsafe extern "system" {
        fn GetProcessMemoryInfo(
            process: Handle,
            counters: *mut ProcessMemoryCountersEx,
            size: u32,
        ) -> Bool;
    }

    struct OwnedHandle(Handle);

    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: `self.0` is an owned kernel handle and is closed once.
                unsafe { CloseHandle(self.0) };
            }
        }
    }

    struct BootstrapArgs {
        powershell: PathBuf,
        supervisor_script: PathBuf,
        working_directory: PathBuf,
        log_path: PathBuf,
    }

    fn parse_args() -> Result<BootstrapArgs, String> {
        let values: Vec<OsString> = std::env::args_os().skip(1).collect();
        if values.len() != 4 {
            return Err(format!(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_ARGUMENTS_INVALID expected=4 actual={} remediation=regenerate the setup-owned Scheduled Task action",
                values.len()
            ));
        }
        let result = BootstrapArgs {
            powershell: PathBuf::from(&values[0]),
            supervisor_script: PathBuf::from(&values[1]),
            working_directory: PathBuf::from(&values[2]),
            log_path: PathBuf::from(&values[3]),
        };
        if !result.powershell.is_absolute()
            || !result.supervisor_script.is_absolute()
            || !result.working_directory.is_absolute()
            || !result.log_path.is_absolute()
            || !result.powershell.is_file()
            || !result.supervisor_script.is_file()
            || !result.working_directory.is_dir()
        {
            return Err(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_PATH_INVALID remediation=all four setup-owned paths must be absolute; PowerShell and supervisor must be regular files and working directory must exist"
                    .to_owned(),
            );
        }
        if [
            result.powershell.as_os_str(),
            result.supervisor_script.as_os_str(),
            result.working_directory.as_os_str(),
            result.log_path.as_os_str(),
        ]
        .iter()
        .any(|value| value.to_str().is_none())
        {
            return Err(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_NON_UNICODE_PATH remediation=install Synapse under Windows paths representable as Unicode; the bootstrap never lossily rewrites command-line bytes"
                    .to_owned(),
            );
        }
        Ok(result)
    }

    fn wide_null(value: &OsStr) -> Vec<u16> {
        value.encode_wide().chain(Some(0)).collect()
    }

    fn quote_windows_argument(value: &OsStr) -> Result<String, String> {
        let value = value.to_str().ok_or_else(|| {
            "SYNAPSE_SUPERVISOR_BOOTSTRAP_NON_UNICODE_PATH remediation=the bootstrap never lossily rewrites command-line bytes"
                .to_owned()
        })?;
        let mut quoted = String::from("\"");
        let mut backslashes = 0usize;
        for character in value.chars() {
            if character == '\\' {
                backslashes += 1;
            } else if character == '"' {
                quoted.push_str(&"\\".repeat(backslashes * 2 + 1));
                quoted.push('"');
                backslashes = 0;
            } else {
                quoted.push_str(&"\\".repeat(backslashes));
                backslashes = 0;
                quoted.push(character);
            }
        }
        quoted.push_str(&"\\".repeat(backslashes * 2));
        quoted.push('"');
        Ok(quoted)
    }

    fn last_error(code: &str) -> String {
        format!(
            "{code} win32={} remediation=inspect Windows Job Object/process creation support; no unbounded supervisor is launched",
            io::Error::last_os_error()
        )
    }

    fn explicit_win32_error(code: &str, raw_error: u32) -> String {
        format!(
            "{code} win32={} remediation=inspect Windows Job Object/process creation support; no unbounded supervisor is launched",
            io::Error::from_raw_os_error(i32::try_from(raw_error).unwrap_or(i32::MAX))
        )
    }

    fn append_log(path: &Path, message: &str) -> io::Result<()> {
        let mut output = OpenOptions::new().create(true).append(true).open(path)?;
        writeln!(
            output,
            "{} {message}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |duration| duration.as_millis())
        )?;
        output.flush()?;
        output.sync_all()
    }

    fn persist_terminal_error(arguments: &BootstrapArgs, primary_error: &str) -> String {
        match append_log(&arguments.log_path, primary_error) {
            Ok(()) => primary_error.to_owned(),
            Err(primary_log_error) => {
                let secondary_path = arguments
                    .working_directory
                    .join("synapse-supervisor-bootstrap-fatal.log");
                let combined = format!(
                    "{primary_error}; primary_log_path={} primary_log_error={primary_log_error}",
                    arguments.log_path.display()
                );
                match append_log(&secondary_path, &combined) {
                    Ok(()) => format!(
                        "{combined}; secondary_log_path={} secondary_log_persisted=true",
                        secondary_path.display()
                    ),
                    Err(secondary_log_error) => format!(
                        "{combined}; secondary_log_path={} secondary_log_error={secondary_log_error}",
                        secondary_path.display()
                    ),
                }
            }
        }
    }

    fn bootstrap_memory_snapshot(stage: &str) -> Result<ProcessMemoryCountersEx, String> {
        let mut counters = ProcessMemoryCountersEx {
            cb: u32::try_from(size_of::<ProcessMemoryCountersEx>()).unwrap_or(u32::MAX),
            ..ProcessMemoryCountersEx::default()
        };
        // SAFETY: the pseudo current-process handle and exact writable ABI are valid.
        if unsafe { GetProcessMemoryInfo(GetCurrentProcess(), &raw mut counters, counters.cb) }
            == FALSE
        {
            return Err(last_error(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_PROCESS_MEMORY_READBACK_FAILED",
            ));
        }
        let reserve = TOTAL_COMMITTED_PRIVATE_POLICY_CEILING_BYTES
            .checked_sub(SYNAPSE_OWNED_TREE_HARD_LIMIT_BYTES)
            .ok_or_else(|| {
                format!(
                    "SYNAPSE_SUPERVISOR_BOOTSTRAP_RESERVE_INVALID outer_limit={SYNAPSE_OWNED_TREE_HARD_LIMIT_BYTES} committed_private_ceiling={TOTAL_COMMITTED_PRIVATE_POLICY_CEILING_BYTES} remediation=compile an owned-tree committed/private limit strictly below the decimal policy ceiling"
                )
            })?;
        let current_private = u64::try_from(counters.private_usage).unwrap_or(u64::MAX);
        let peak_commit = u64::try_from(counters.peak_pagefile_usage).unwrap_or(u64::MAX);
        let current_working_set = u64::try_from(counters.working_set_size).unwrap_or(u64::MAX);
        let peak_working_set = u64::try_from(counters.peak_working_set_size).unwrap_or(u64::MAX);
        let conservative_bootstrap_commit_bytes = current_private.max(peak_commit);
        if reserve == 0
            || conservative_bootstrap_commit_bytes > reserve
            || SYNAPSE_OWNED_TREE_HARD_LIMIT_BYTES
                .checked_add(conservative_bootstrap_commit_bytes)
                .is_none_or(|value| value > TOTAL_COMMITTED_PRIVATE_POLICY_CEILING_BYTES)
        {
            return Err(format!(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_PREASSOCIATION_RESERVE_EXCEEDED stage={stage} current_private_bytes={current_private} peak_commit_bytes={peak_commit} current_working_set_bytes={current_working_set} peak_working_set_bytes={peak_working_set} working_set_policy=measured_only conservative_bootstrap_commit_bytes={conservative_bootstrap_commit_bytes} reserve_bytes={reserve} outer_job_limit_bytes={SYNAPSE_OWNED_TREE_HARD_LIMIT_BYTES} committed_private_ceiling_bytes={TOTAL_COMMITTED_PRIVATE_POLICY_CEILING_BYTES} remediation=optimize the native bootstrap before launching managed supervisor code; never increase the combined committed/private ceiling"
            ));
        }
        Ok(counters)
    }

    fn set_and_verify_parent_job(job: Handle) -> Result<JobObjectMemoryUsageInformation, String> {
        let mut limits = JobObjectExtendedLimitInformation::default();
        limits.basic_limit_information.limit_flags = EXPECTED_JOB_LIMIT_FLAGS;
        let expected_memory =
            usize::try_from(SYNAPSE_OWNED_TREE_HARD_LIMIT_BYTES).map_err(|error| {
                format!("SYNAPSE_SUPERVISOR_BOOTSTRAP_LIMIT_CONVERSION_FAILED error={error}")
            })?;
        limits.process_memory_limit = expected_memory;
        limits.job_memory_limit = expected_memory;
        // SAFETY: `limits` is the documented class-9 ABI and length.
        if unsafe {
            SetInformationJobObject(
                job,
                JOB_OBJECT_EXTENDED_LIMIT_INFORMATION,
                (&raw const limits).cast(),
                u32::try_from(size_of::<JobObjectExtendedLimitInformation>()).unwrap_or(u32::MAX),
            )
        } == FALSE
        {
            return Err(last_error(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_JOB_LIMIT_SET_FAILED",
            ));
        }
        let cpu = JobObjectCpuRateControlInformation {
            control_flags: EXPECTED_CPU_RATE_FLAGS,
            cpu_rate: SYNAPSE_OWNED_TREE_CPU_RATE,
        };
        // SAFETY: `cpu` is the documented class-15 ABI and length.
        if unsafe {
            SetInformationJobObject(
                job,
                JOB_OBJECT_CPU_RATE_CONTROL_INFORMATION,
                (&raw const cpu).cast(),
                u32::try_from(size_of::<JobObjectCpuRateControlInformation>()).unwrap_or(u32::MAX),
            )
        } == FALSE
        {
            return Err(last_error(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_CPU_RATE_SET_FAILED",
            ));
        }
        // SAFETY: pseudo current-process handle and owned Job handle are valid.
        if unsafe { AssignProcessToJobObject(job, GetCurrentProcess()) } == FALSE {
            return Err(last_error(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_ASSIGN_SELF_FAILED",
            ));
        }
        let mut in_job = FALSE;
        // SAFETY: pointers are valid for the call.
        if unsafe { IsProcessInJob(GetCurrentProcess(), job, &raw mut in_job) } == FALSE
            || in_job == FALSE
        {
            return Err(last_error(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_MEMBERSHIP_READBACK_FAILED",
            ));
        }

        let mut limit_readback = JobObjectExtendedLimitInformation::default();
        let mut returned = 0u32;
        let expected_limit_size =
            u32::try_from(size_of::<JobObjectExtendedLimitInformation>()).unwrap_or(u32::MAX);
        // SAFETY: readback buffer is exact class-9 storage.
        if unsafe {
            QueryInformationJobObject(
                job,
                JOB_OBJECT_EXTENDED_LIMIT_INFORMATION,
                (&raw mut limit_readback).cast(),
                expected_limit_size,
                &raw mut returned,
            )
        } == FALSE
        {
            return Err(last_error(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_JOB_LIMIT_READBACK_FAILED",
            ));
        }
        if returned != expected_limit_size {
            return Err(format!(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_JOB_LIMIT_READBACK_SIZE_MISMATCH expected={expected_limit_size} actual={returned} remediation=the host ABI must return the exact class-9 structure"
            ));
        }
        let mut cpu_readback = JobObjectCpuRateControlInformation::default();
        returned = 0;
        let expected_cpu_size =
            u32::try_from(size_of::<JobObjectCpuRateControlInformation>()).unwrap_or(u32::MAX);
        // SAFETY: readback buffer is exact class-15 storage.
        if unsafe {
            QueryInformationJobObject(
                job,
                JOB_OBJECT_CPU_RATE_CONTROL_INFORMATION,
                (&raw mut cpu_readback).cast(),
                expected_cpu_size,
                &raw mut returned,
            )
        } == FALSE
        {
            return Err(last_error(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_CPU_RATE_READBACK_FAILED",
            ));
        }
        if returned != expected_cpu_size {
            return Err(format!(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_CPU_RATE_READBACK_SIZE_MISMATCH expected={expected_cpu_size} actual={returned} remediation=the host ABI must return the exact class-15 structure"
            ));
        }
        if limit_readback.basic_limit_information.limit_flags != EXPECTED_JOB_LIMIT_FLAGS
            || limit_readback.process_memory_limit != expected_memory
            || limit_readback.job_memory_limit != expected_memory
            || cpu_readback.control_flags != EXPECTED_CPU_RATE_FLAGS
            || cpu_readback.cpu_rate != SYNAPSE_OWNED_TREE_CPU_RATE
        {
            return Err(format!(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_JOB_CONTRACT_DRIFT flags=0x{:08X} process_memory={} job_memory={} cpu_flags=0x{:08X} cpu_rate={} working_set_policy=measured_only remediation=the kernel did not retain the exact compiled parent Job contract",
                limit_readback.basic_limit_information.limit_flags,
                limit_readback.process_memory_limit,
                limit_readback.job_memory_limit,
                cpu_readback.control_flags,
                cpu_readback.cpu_rate
            ));
        }
        let mut memory = JobObjectMemoryUsageInformation::default();
        returned = 0;
        // SAFETY: raw class 28 is the exact two-u64 ABI used by Windows/hcsshim.
        if unsafe {
            QueryInformationJobObject(
                job,
                JOB_OBJECT_MEMORY_USAGE_INFORMATION,
                (&raw mut memory).cast(),
                u32::try_from(size_of::<JobObjectMemoryUsageInformation>()).unwrap_or(u32::MAX),
                &raw mut returned,
            )
        } == FALSE
            || usize::try_from(returned).ok() != Some(size_of::<JobObjectMemoryUsageInformation>())
            || memory.job_memory > SYNAPSE_OWNED_TREE_HARD_LIMIT_BYTES
            || memory.peak_job_memory_used > SYNAPSE_OWNED_TREE_HARD_LIMIT_BYTES
            || memory.peak_job_memory_used < memory.job_memory
            || memory.peak_job_memory_used
                < u64::try_from(limit_readback.peak_job_memory_used).unwrap_or(u64::MAX)
        {
            return Err(format!(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_JOB_MEMORY_READBACK_FAILED returned={returned} current={} peak={} earlier_extended_peak={} limit={} os_error={} remediation=class-28 accounting must be exact, within the compiled parent limit, and monotone relative to the earlier class-9 readback",
                memory.job_memory,
                memory.peak_job_memory_used,
                limit_readback.peak_job_memory_used,
                SYNAPSE_OWNED_TREE_HARD_LIMIT_BYTES,
                io::Error::last_os_error()
            ));
        }
        Ok(memory)
    }

    fn terminate_child_and_verify(process: Handle, expected_exit_code: u32) -> Result<(), String> {
        // SAFETY: the caller owns this exact child process handle. Terminating
        // only the suspended child preserves the bootstrap long enough for
        // `main` to record the primary failure before process teardown closes
        // the sole KILL_ON_JOB_CLOSE parent-Job handle.
        if unsafe { TerminateProcess(process, expected_exit_code) } == FALSE {
            return Err(last_error(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_CHILD_TERMINATE_FAILED",
            ));
        }
        // SAFETY: the child process handle remains live and waitable here.
        let wait = unsafe { WaitForSingleObject(process, CHILD_TERMINATION_WAIT_MS) };
        if wait == WAIT_FAILED {
            return Err(last_error(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_CHILD_TERMINATION_WAIT_FAILED",
            ));
        }
        if wait == WAIT_TIMEOUT {
            return Err(format!(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_CHILD_TERMINATION_TIMEOUT timeout_ms={CHILD_TERMINATION_WAIT_MS} remediation=inspect the exact suspended supervisor child and Windows process termination state"
            ));
        }
        if wait != WAIT_OBJECT_0 {
            return Err(format!(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_CHILD_TERMINATION_WAIT_UNEXPECTED result={wait} remediation=inspect the exact suspended supervisor child and Windows wait state"
            ));
        }
        let mut actual_exit_code = 0u32;
        // SAFETY: the successful wait proved this exact process is signaled.
        if unsafe { GetExitCodeProcess(process, &raw mut actual_exit_code) } == FALSE {
            return Err(last_error(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_CHILD_TERMINATION_EXIT_READBACK_FAILED",
            ));
        }
        if actual_exit_code != expected_exit_code {
            return Err(format!(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_CHILD_TERMINATION_EXIT_MISMATCH expected={expected_exit_code} actual={actual_exit_code} remediation=refuse an unverified child cleanup outcome"
            ));
        }
        Ok(())
    }

    fn run(arguments: &BootstrapArgs) -> Result<u32, String> {
        // SAFETY: obtaining the current PID has no preconditions.
        let process_id = unsafe { GetCurrentProcessId() };
        let job_name = format!("Local\\SynapseOwned-{process_id}");
        let wide_job_name = wide_null(OsStr::new(&job_name));
        let wide_sddl = wide_null(OsStr::new(JOB_SECURITY_DESCRIPTOR_SDDL));
        let mut security_descriptor = null_mut();
        let mut security_descriptor_size = 0u32;
        // SAFETY: SDDL is terminated and output pointers are valid.
        if unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                wide_sddl.as_ptr(),
                SECURITY_DESCRIPTOR_REVISION,
                &raw mut security_descriptor,
                &raw mut security_descriptor_size,
            )
        } == FALSE
            || security_descriptor.is_null()
            || security_descriptor_size == 0
        {
            return Err(last_error(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_JOB_SECURITY_DESCRIPTOR_FAILED",
            ));
        }
        let security_attributes = SecurityAttributes {
            length: u32::try_from(size_of::<SecurityAttributes>()).unwrap_or(u32::MAX),
            security_descriptor,
            inherit_handle: FALSE,
        };
        // SAFETY: name and exact security attributes remain live for creation.
        let raw_job =
            unsafe { CreateJobObjectW(&raw const security_attributes, wide_job_name.as_ptr()) };
        // SAFETY: GetLastError must immediately follow CreateJobObjectW because a
        // non-null return can still mean the named Job already existed.
        let create_job_error = unsafe { GetLastError() };
        // SAFETY: the descriptor was allocated by LocalAlloc through Advapi32.
        unsafe { LocalFree(security_descriptor) };
        if raw_job.is_null() {
            return Err(explicit_win32_error(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_JOB_CREATE_FAILED",
                create_job_error,
            ));
        }
        if create_job_error == ERROR_ALREADY_EXISTS {
            // SAFETY: this handle was returned by CreateJobObjectW and is not adopted.
            unsafe { CloseHandle(raw_job) };
            return Err(format!(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_JOB_NAME_COLLISION job_name={job_name} win32={create_job_error} remediation=refuse ambiguous kernel ownership; inspect a stale same-PID Job before restarting"
            ));
        }
        // This is the sole lifetime-owning handle for a Job configured with
        // KILL_ON_JOB_CLOSE. Returning from `run` must not close it before
        // `main` records the terminal outcome and selects the exact exit code.
        // `main` always terminates via `process::exit`, so Windows closes this
        // deliberately retained handle at process teardown and then applies
        // KILL_ON_JOB_CLOSE to any descendants that are still alive.
        let job = ManuallyDrop::new(OwnedHandle(raw_job));
        let memory = set_and_verify_parent_job(job.0)?;
        let bootstrap_memory = bootstrap_memory_snapshot("after_parent_bind")?;
        append_log(
            &arguments.log_path,
            &format!(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_PARENT_JOB_BOUND bootstrap_pid={process_id} job_name={job_name} job_security_descriptor_sddl={JOB_SECURITY_DESCRIPTOR_SDDL} flags=0x{EXPECTED_JOB_LIMIT_FLAGS:08X} process_memory_limit_bytes={SYNAPSE_OWNED_TREE_HARD_LIMIT_BYTES} job_memory_limit_bytes={SYNAPSE_OWNED_TREE_HARD_LIMIT_BYTES} working_set_policy=measured_only cpu_flags=0x{EXPECTED_CPU_RATE_FLAGS:08X} cpu_rate={SYNAPSE_OWNED_TREE_CPU_RATE} current_job_memory_bytes={} peak_job_memory_bytes={} bootstrap_current_private_bytes={} bootstrap_peak_commit_bytes={} bootstrap_current_working_set_bytes={} bootstrap_peak_working_set_bytes={} bootstrap_reserve_bytes={}",
                memory.job_memory,
                memory.peak_job_memory_used,
                bootstrap_memory.private_usage,
                bootstrap_memory.peak_pagefile_usage,
                bootstrap_memory.working_set_size,
                bootstrap_memory.peak_working_set_size,
                TOTAL_COMMITTED_PRIVATE_POLICY_CEILING_BYTES
                    - SYNAPSE_OWNED_TREE_HARD_LIMIT_BYTES
            ),
        )
        .map_err(|error| format!("SYNAPSE_SUPERVISOR_BOOTSTRAP_LOG_WRITE_FAILED error={error}"))?;

        let mut command = format!(
            "{} -NoProfile -ExecutionPolicy Bypass -File {} -ParentJobName {}",
            quote_windows_argument(arguments.powershell.as_os_str())?,
            quote_windows_argument(arguments.supervisor_script.as_os_str())?,
            quote_windows_argument(OsStr::new(&job_name))?
        );
        let mut wide_command: Vec<u16> =
            OsStr::new(&command).encode_wide().chain(Some(0)).collect();
        command.clear();
        let wide_application = wide_null(arguments.powershell.as_os_str());
        let wide_directory = wide_null(arguments.working_directory.as_os_str());
        // SAFETY: zero is the documented initialization for these Win32 structs.
        let mut startup: StartupInfoW = unsafe { zeroed() };
        startup.cb = u32::try_from(size_of::<StartupInfoW>()).unwrap_or(u32::MAX);
        // SAFETY: zero is the documented initialization for this output struct.
        let mut process: ProcessInformation = unsafe { zeroed() };
        // SAFETY: all pointers reference live, correctly sized buffers for the call.
        if unsafe {
            CreateProcessW(
                wide_application.as_ptr(),
                wide_command.as_mut_ptr(),
                null(),
                null(),
                FALSE,
                CREATE_SUSPENDED | CREATE_NO_WINDOW,
                null(),
                wide_directory.as_ptr(),
                &raw mut startup,
                &raw mut process,
            )
        } == FALSE
        {
            return Err(last_error(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_CREATE_PROCESS_FAILED",
            ));
        }
        let child_process = OwnedHandle(process.process);
        let child_thread = OwnedHandle(process.thread);
        let mut in_job = FALSE;
        // SAFETY: exact live process and parent Job handles are queried.
        let membership_query = unsafe { IsProcessInJob(child_process.0, job.0, &raw mut in_job) };
        if membership_query == FALSE || in_job == FALSE {
            let cause = if membership_query == FALSE {
                last_error("SYNAPSE_SUPERVISOR_BOOTSTRAP_CHILD_JOB_MEMBERSHIP_FAILED")
            } else {
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_CHILD_JOB_MEMBERSHIP_FAILED in_job=false remediation=refuse to resume a supervisor that was not creation-time associated with the authoritative parent Job".to_owned()
            };
            terminate_child_and_verify(child_process.0, 125)
                .map_err(|cleanup| format!("{cause}; child_cleanup_error={cleanup}"))?;
            return Err(cause);
        }
        // SAFETY: the primary thread remains suspended and owned here.
        if unsafe { ResumeThread(child_thread.0) } == u32::MAX {
            let cause = last_error("SYNAPSE_SUPERVISOR_BOOTSTRAP_RESUME_FAILED");
            terminate_child_and_verify(child_process.0, 126)
                .map_err(|cleanup| format!("{cause}; child_cleanup_error={cleanup}"))?;
            return Err(cause);
        }
        append_log(
            &arguments.log_path,
            &format!(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_CHILD_STARTED bootstrap_pid={process_id} supervisor_pid={} job_name={job_name} creation_assignment=inherited_parent_job exact_membership_readback=true",
                process.process_id
            ),
        )
        .map_err(|error| format!("SYNAPSE_SUPERVISOR_BOOTSTRAP_LOG_WRITE_FAILED error={error}"))?;
        // SAFETY: the child process handle remains live for the whole wait.
        let wait = unsafe { WaitForSingleObject(child_process.0, INFINITE) };
        if wait == WAIT_FAILED || wait != WAIT_OBJECT_0 {
            return Err(last_error("SYNAPSE_SUPERVISOR_BOOTSTRAP_WAIT_FAILED"));
        }
        let mut exit_code = 0u32;
        // SAFETY: wait proved the process is signaled and handle is valid.
        if unsafe { GetExitCodeProcess(child_process.0, &raw mut exit_code) } == FALSE {
            return Err(last_error(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_EXIT_READBACK_FAILED",
            ));
        }
        let final_bootstrap_memory = bootstrap_memory_snapshot("after_supervisor_exit")?;
        append_log(
            &arguments.log_path,
            &format!(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_CHILD_EXIT bootstrap_pid={process_id} supervisor_pid={} exit_code={exit_code} bootstrap_current_private_bytes={} bootstrap_peak_commit_bytes={} bootstrap_current_working_set_bytes={} bootstrap_peak_working_set_bytes={} bootstrap_reserve_bytes={}",
                process.process_id,
                final_bootstrap_memory.private_usage,
                final_bootstrap_memory.peak_pagefile_usage,
                final_bootstrap_memory.working_set_size,
                final_bootstrap_memory.peak_working_set_size,
                TOTAL_COMMITTED_PRIVATE_POLICY_CEILING_BYTES
                    - SYNAPSE_OWNED_TREE_HARD_LIMIT_BYTES
            ),
        )
        .map_err(|error| format!("SYNAPSE_SUPERVISOR_BOOTSTRAP_LOG_WRITE_FAILED error={error}"))?;
        Ok(exit_code)
    }

    pub fn main() {
        let arguments = match parse_args() {
            Ok(arguments) => arguments,
            Err(error) => {
                eprintln!("{error}");
                std::process::exit(2);
            }
        };
        match run(&arguments) {
            Ok(exit_code) => std::process::exit(i32::from_ne_bytes(exit_code.to_ne_bytes())),
            Err(error) => {
                let durable_error = persist_terminal_error(&arguments, &error);
                eprintln!("{durable_error}");
                std::process::exit(1);
            }
        }
    }
}

#[cfg(windows)]
fn main() {
    windows_bootstrap::main();
}
