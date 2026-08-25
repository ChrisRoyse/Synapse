#![cfg_attr(windows, windows_subsystem = "windows")]

//! Minimal native bootstrap for the installed Windows daemon supervisor.
//!
//! Task Scheduler starts this binary instead of PowerShell. Production outer
//! launches use an explicit capability argument and bind themselves to a
//! capability-qualified whole-owned-tree Job before any managed runtime starts.
//! The exact four-path ABI remains a V2-attested, non-authoritative probe mode.
//! An explicit `--nested-job` mode creates a 100%-of-parent ownership Job only
//! after proving its capability-qualified inherited whole-tree Job. In every
//! mode, the PowerShell child inherits the new Job at process creation. After
//! containment, the bootstrap attests its mapped main image plus retained
//! read-only PowerShell and script handles against the script's content-addressed
//! V2 launch contract. It proves exact child Job membership and mapped image
//! while the child is suspended, resumes it, and retains the owned Job and
//! authoritative artifact leases through process teardown.

#[cfg(not(windows))]
fn main() {
    eprintln!(
        "SYNAPSE_SUPERVISOR_BOOTSTRAP_UNSUPPORTED_PLATFORM: Windows Job Objects are required; remediation=run the installed supervisor bootstrap only on Windows"
    );
    std::process::exit(1);
}

#[cfg(windows)]
mod windows_bootstrap {
    use std::cmp::Ordering;
    use std::ffi::{OsStr, OsString, c_void};
    use std::fs::{OpenOptions, create_dir_all};
    use std::io::{self, Write};
    use std::mem::{ManuallyDrop, size_of, zeroed};
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use std::path::{Path, PathBuf};
    use std::ptr::{null, null_mut};
    use std::time::Instant;

    use serde::{Deserialize, Serialize};
    use sha2::{Digest, Sha256};
    use synapse_core::{SYNAPSE_OWNED_TREE_CPU_RATE, SYNAPSE_OWNED_TREE_HARD_LIMIT_BYTES};

    type Handle = *mut c_void;
    type Bool = i32;

    const FALSE: Bool = 0;
    const TRUE: Bool = 1;
    const DEBUG_ONLY_THIS_PROCESS: u32 = 0x0000_0002;
    const CREATE_SUSPENDED: u32 = 0x0000_0004;
    const CREATE_UNICODE_ENVIRONMENT: u32 = 0x0000_0400;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const GENERIC_READ: u32 = 0x8000_0000;
    const FILE_SHARE_READ: u32 = 0x0000_0001;
    const OPEN_EXISTING: u32 = 3;
    const FILE_ATTRIBUTE_NORMAL: u32 = 0x0000_0080;
    const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x0000_0010;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const FILE_ID_INFO_CLASS: i32 = 18;
    const VOLUME_NAME_NT: u32 = 0x0000_0002;
    const JOB_OBJECT_QUERY: u32 = 0x0000_0004;
    const JOB_OBJECT_BASIC_PROCESS_ID_LIST: i32 = 3;
    const PROCESS_QUERY_INFORMATION: u32 = 0x0000_0400;
    const PROCESS_VM_READ: u32 = 0x0000_0010;
    const TH32CS_SNAPPROCESS: u32 = 0x0000_0002;
    const LIST_MODULES_ALL: u32 = 0x0000_0003;
    const ERROR_INVALID_HANDLE: u32 = 6;
    const ERROR_ALREADY_EXISTS: u32 = 183;
    const ERROR_NO_MORE_FILES: u32 = 18;
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
    const CREATE_THREAD_DEBUG_EVENT: u32 = 2;
    const CREATE_PROCESS_DEBUG_EVENT: u32 = 3;
    const EXIT_PROCESS_DEBUG_EVENT: u32 = 5;
    const LOAD_DLL_DEBUG_EVENT: u32 = 6;
    const DBG_CONTINUE: u32 = 0x0001_0002;
    const STILL_ACTIVE: u32 = 259;
    const CHILD_DEBUG_EVENT_WAIT_MS: u32 = 30_000;
    const CHILD_TERMINATION_WAIT_MS: u32 = 30_000;
    const UNPROVED_CHILD_CLEANUP_EXIT_CODE: u32 = 130;
    const MAX_DEBUG_CLEANUP_EVENTS: usize = 32;
    const FILE_BEGIN: u32 = 0;
    const SECURITY_DESCRIPTOR_REVISION: u32 = 1;
    const KF_FLAG_DONT_VERIFY: u32 = 0x0000_4000;
    const MAX_KNOWN_FOLDER_PATH_UNITS: usize = 32_767;
    const MAX_WINDOWS_PATH_UNITS: usize = 32_767;
    const MAX_LAUNCH_CONTRACT_HEADER_BYTES: usize = 65_536;
    const MAX_ATTESTED_FILE_BYTES: u64 = 256 * 1024 * 1024;
    const MAX_PROCESS_MODULES: usize = 512;
    const MAX_PROCESS_SNAPSHOT_ENTRIES: usize = 16_384;
    const MAX_JOB_PROCESS_IDS: usize = 1_024;
    const JOB_PROCESS_LIST_STABILITY_ATTEMPTS: usize = 4;
    const TOTAL_COMMITTED_PRIVATE_POLICY_CEILING_BYTES: u64 = 1_000_000_000;
    const NESTED_JOB_CPU_RATE: u32 = 10_000;
    const JOB_SECURITY_DESCRIPTOR_SDDL: &str = "D:P(A;;GA;;;SY)(A;;0x00100004;;;OW)";
    const UTF8_BOM: [u8; 3] = [0xEF, 0xBB, 0xBF];
    const LAUNCH_CONTRACT_PREFIX: &str = "# SYNAPSE_SUPERVISOR_LAUNCH_CONTRACT_V2 ";
    const LAUNCH_CONTRACT_SCHEMA: &str = "synapse_supervisor_launch_contract/v2";
    const CAPABILITY_JOB_NAME_TEMPLATE: &str =
        "Local\\SynapseOwned-{LAUNCH_CAPABILITY_SHA256}-{SELF_PID}";
    const LEGACY_JOB_NAME_TEMPLATE: &str = "Local\\SynapseOwned-{SELF_PID}";
    const OUTER_INHERITED_JOB: &str = "none";
    const INNER_INHERITED_JOB: &str = "outer_complete_tree";
    const INNER_INHERITED_JOB_NAME_TEMPLATE: &str =
        "Local\\SynapseOwned-{OUTER_LAUNCH_CAPABILITY_SHA256}-{OUTER_BOOTSTRAP_PID}";
    const BOOTSTRAP_LEAF_PREFIX: &str = "broker-bootstrap-";
    const OUTER_SCRIPT_LEAF_PREFIX: &str = "broker-supervisor-";
    const INNER_SCRIPT_LEAF_PREFIX: &str = "authority-supervisor-";
    const LEGACY_SCRIPT_LEAF_PREFIX: &str = "probe-supervisor-";
    const CONTRACT_ENV_KEYS: [&str; 25] = [
        "SYNAPSE_LAUNCH_CONTRACT_SCHEMA",
        "SYNAPSE_LAUNCH_ROLE",
        "SYNAPSE_LAUNCH_MODE",
        "SYNAPSE_BOOTSTRAP_PATH",
        "SYNAPSE_BOOTSTRAP_VOLUME_SERIAL",
        "SYNAPSE_BOOTSTRAP_FILE_ID_128",
        "SYNAPSE_BOOTSTRAP_SHA256",
        "SYNAPSE_BOOTSTRAP_LENGTH",
        "SYNAPSE_POWERSHELL_PATH",
        "SYNAPSE_POWERSHELL_VOLUME_SERIAL",
        "SYNAPSE_POWERSHELL_FILE_ID_128",
        "SYNAPSE_POWERSHELL_SHA256",
        "SYNAPSE_POWERSHELL_LENGTH",
        "SYNAPSE_SCRIPT_PATH",
        "SYNAPSE_SCRIPT_VOLUME_SERIAL",
        "SYNAPSE_SCRIPT_FILE_ID_128",
        "SYNAPSE_SCRIPT_SHA256",
        "SYNAPSE_SCRIPT_LENGTH",
        "SYNAPSE_BROKER_IDENTITY_SHA256",
        "SYNAPSE_LAUNCH_CAPABILITY_SHA256",
        "SYNAPSE_OUTER_LAUNCH_CAPABILITY_SHA256",
        "SYNAPSE_AUTHORITY_EPOCH",
        "SYNAPSE_CREATED_JOB_NAME",
        "SYNAPSE_EXPECTED_INHERITED_JOB",
        "SYNAPSE_EXPECTED_INHERITED_JOB_NAME",
    ];
    const EXPECTED_RATE_ENV_KEY: &str = "SYNAPSE_EXPECTED_JOB_CPU_RATE";
    const _: () = assert!(SYNAPSE_OWNED_TREE_CPU_RATE == 2_500);
    const _: () = assert!(SYNAPSE_OWNED_TREE_HARD_LIMIT_BYTES == 949_997_568);

    #[repr(C)]
    struct Guid {
        data_1: u32,
        data_2: u16,
        data_3: u16,
        data_4: [u8; 8],
    }

    static FOLDER_ID_LOCAL_APP_DATA: Guid = Guid {
        data_1: 0xF1B3_2785,
        data_2: 0x6FBA,
        data_3: 0x4FCF,
        data_4: [0x9D, 0x55, 0x7B, 0x8E, 0x7F, 0x15, 0x70, 0x91],
    };

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

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct ExceptionRecord {
        exception_code: u32,
        exception_flags: u32,
        exception_record: *mut Self,
        exception_address: *mut c_void,
        number_parameters: u32,
        exception_information: [usize; 15],
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct ExceptionDebugInfo {
        exception_record: ExceptionRecord,
        first_chance: u32,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct CreateProcessDebugInfo {
        file: Handle,
        process: Handle,
        thread: Handle,
        base_of_image: *mut c_void,
        debug_info_file_offset: u32,
        debug_info_size: u32,
        thread_local_base: *mut c_void,
        start_address: *mut c_void,
        image_name: *mut c_void,
        unicode: u16,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct CreateThreadDebugInfo {
        thread: Handle,
        thread_local_base: *mut c_void,
        start_address: *mut c_void,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct LoadDllDebugInfo {
        file: Handle,
        base_of_dll: *mut c_void,
        debug_info_file_offset: u32,
        debug_info_size: u32,
        image_name: *mut c_void,
        unicode: u16,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    union DebugEventInfo {
        exception: ExceptionDebugInfo,
        create_thread: CreateThreadDebugInfo,
        create_process: CreateProcessDebugInfo,
        load_dll: LoadDllDebugInfo,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct DebugEvent {
        code: u32,
        process_id: u32,
        thread_id: u32,
        info: DebugEventInfo,
    }

    #[cfg(target_pointer_width = "64")]
    const _: () = assert!(size_of::<DebugEvent>() == 176);
    #[cfg(target_pointer_width = "32")]
    const _: () = assert!(size_of::<DebugEvent>() == 96);

    #[repr(C)]
    #[derive(Clone, Copy, Default, Eq, PartialEq)]
    struct FileTime {
        low_date_time: u32,
        high_date_time: u32,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default, Eq, PartialEq)]
    struct ByHandleFileInformation {
        file_attributes: u32,
        creation_time: FileTime,
        last_access_time: FileTime,
        last_write_time: FileTime,
        volume_serial_number: u32,
        file_size_high: u32,
        file_size_low: u32,
        number_of_links: u32,
        file_index_high: u32,
        file_index_low: u32,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default, Eq, PartialEq)]
    struct FileIdInfo {
        volume_serial_number: u64,
        file_id: [u8; 16],
    }

    #[repr(C)]
    struct ProcessEntry32W {
        size: u32,
        usage_count: u32,
        process_id: u32,
        default_heap_id: usize,
        module_id: u32,
        thread_count: u32,
        parent_process_id: u32,
        base_priority: i32,
        flags: u32,
        executable_file: [u16; 260],
    }

    #[repr(C)]
    struct JobObjectBasicProcessIdList {
        number_of_assigned_processes: u32,
        number_of_process_ids_in_list: u32,
        process_id_list: [usize; MAX_JOB_PROCESS_IDS],
    }

    #[derive(Clone)]
    struct InheritedParentDescriptor {
        job_name: String,
        broker_identity_sha256: String,
        outer_launch_capability_sha256: String,
        outer_bootstrap_pid: u32,
    }

    struct ParentJobCandidate {
        descriptor: InheritedParentDescriptor,
        job: OwnedHandle,
    }

    struct ProcessImageEvidence {
        process_id: u32,
        creation_time: u64,
        full_dos_path: PathBuf,
        mapped_nt_path: OsString,
    }

    struct InheritedProcessLineage {
        broker_process_id: u32,
        outer_process: OwnedHandle,
        broker_process: OwnedHandle,
        outer_image: ProcessImageEvidence,
        broker_image: ProcessImageEvidence,
        current_creation_time: u64,
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentProcess() -> Handle;
        fn GetCurrentProcessId() -> u32;
        fn GetProcessId(process: Handle) -> u32;
        fn GetThreadId(thread: Handle) -> u32;
        fn OpenProcess(desired_access: u32, inherit_handle: Bool, process_id: u32) -> Handle;
        fn GetProcessTimes(
            process: Handle,
            creation_time: *mut FileTime,
            exit_time: *mut FileTime,
            kernel_time: *mut FileTime,
            user_time: *mut FileTime,
        ) -> Bool;
        fn GetThreadTimes(
            thread: Handle,
            creation_time: *mut FileTime,
            exit_time: *mut FileTime,
            kernel_time: *mut FileTime,
            user_time: *mut FileTime,
        ) -> Bool;
        fn QueryFullProcessImageNameW(
            process: Handle,
            flags: u32,
            executable_name: *mut u16,
            size: *mut u32,
        ) -> Bool;
        fn CreateToolhelp32Snapshot(flags: u32, process_id: u32) -> Handle;
        fn Process32FirstW(snapshot: Handle, entry: *mut ProcessEntry32W) -> Bool;
        fn Process32NextW(snapshot: Handle, entry: *mut ProcessEntry32W) -> Bool;
        fn GetModuleHandleW(module_name: *const u16) -> Handle;
        fn GetModuleFileNameW(module: Handle, file_name: *mut u16, size: u32) -> u32;
        fn CreateFileW(
            file_name: *const u16,
            desired_access: u32,
            share_mode: u32,
            security_attributes: *const c_void,
            creation_disposition: u32,
            flags_and_attributes: u32,
            template_file: Handle,
        ) -> Handle;
        fn GetFileInformationByHandle(
            file: Handle,
            information: *mut ByHandleFileInformation,
        ) -> Bool;
        fn GetFileInformationByHandleEx(
            file: Handle,
            information_class: i32,
            information: *mut c_void,
            buffer_size: u32,
        ) -> Bool;
        fn GetFileSizeEx(file: Handle, size: *mut i64) -> Bool;
        fn SetFilePointerEx(
            file: Handle,
            distance_to_move: i64,
            new_file_pointer: *mut i64,
            move_method: u32,
        ) -> Bool;
        fn ReadFile(
            file: Handle,
            buffer: *mut c_void,
            bytes_to_read: u32,
            bytes_read: *mut u32,
            overlapped: *mut c_void,
        ) -> Bool;
        fn GetFinalPathNameByHandleW(
            file: Handle,
            file_path: *mut u16,
            file_path_size: u32,
            flags: u32,
        ) -> u32;
        fn GetVolumeInformationByHandleW(
            file: Handle,
            volume_name_buffer: *mut u16,
            volume_name_size: u32,
            volume_serial_number: *mut u32,
            maximum_component_length: *mut u32,
            file_system_flags: *mut u32,
            file_system_name_buffer: *mut u16,
            file_system_name_size: u32,
        ) -> Bool;
        fn CompareStringOrdinal(
            string_1: *const u16,
            count_1: i32,
            string_2: *const u16,
            count_2: i32,
            ignore_case: Bool,
        ) -> i32;
        fn OpenJobObjectW(desired_access: u32, inherit_handle: Bool, name: *const u16) -> Handle;
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
        fn WaitForDebugEvent(debug_event: *mut DebugEvent, milliseconds: u32) -> Bool;
        fn ContinueDebugEvent(process_id: u32, thread_id: u32, continue_status: u32) -> Bool;
        fn DebugActiveProcessStop(process_id: u32) -> Bool;
        fn WaitForSingleObject(handle: Handle, milliseconds: u32) -> u32;
        fn GetExitCodeProcess(process: Handle, exit_code: *mut u32) -> Bool;
        fn TerminateProcess(process: Handle, exit_code: u32) -> Bool;
        fn CloseHandle(handle: Handle) -> Bool;
        fn LocalFree(memory: *mut c_void) -> *mut c_void;
    }

    unsafe fn compare_object_handles(first_object: Handle, second_object: Handle) -> Bool {
        // The Microsoft import-library contract names Kernelbase.lib, but the
        // Windows SDK installed on supported hosts need not ship that private
        // import library. The windows crate binds the public API-set DLL
        // directly, which is the stable Win32 contract and avoids inventing an
        // SDK file prerequisite. Keep the raw-handle conversion at this single
        // boundary so every caller still observes the native BOOL/GetLastError
        // behavior without a fallback implementation.
        unsafe {
            windows::Win32::Foundation::CompareObjectHandles(
                windows::Win32::Foundation::HANDLE(first_object),
                windows::Win32::Foundation::HANDLE(second_object),
            )
            .0
        }
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
        fn EnumProcessModulesEx(
            process: Handle,
            modules: *mut Handle,
            modules_size: u32,
            needed: *mut u32,
            filter_flag: u32,
        ) -> Bool;
        fn GetMappedFileNameW(
            process: Handle,
            module: Handle,
            file_name: *mut u16,
            size: u32,
        ) -> u32;
        fn GetProcessMemoryInfo(
            process: Handle,
            counters: *mut ProcessMemoryCountersEx,
            size: u32,
        ) -> Bool;
    }

    #[link(name = "shell32")]
    unsafe extern "system" {
        fn SHGetKnownFolderPath(
            folder_id: *const Guid,
            flags: u32,
            token: Handle,
            path: *mut *mut u16,
        ) -> i32;
    }

    #[link(name = "ole32")]
    unsafe extern "system" {
        fn CoTaskMemFree(memory: *const c_void);
    }

    struct OwnedHandle(Handle);

    impl OwnedHandle {
        fn close_checked(mut self, code: &str) -> Result<(), String> {
            if self.0.is_null() || is_invalid_handle(self.0) {
                return Err(format!(
                    "{code} handle=NULL remediation=refuse an unproved handle-close boundary"
                ));
            }
            // SAFETY: `self.0` is this instance's exact owned kernel handle.
            if unsafe { CloseHandle(self.0) } == FALSE {
                let error = last_error(code);
                // Leave the raw handle installed so Drop makes one final
                // best-effort close before the failure is routed durably.
                return Err(error);
            }
            self.0 = null_mut();
            Ok(())
        }
    }

    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: `self.0` is an owned kernel handle and is closed once.
                unsafe { CloseHandle(self.0) };
            }
        }
    }

    #[derive(Clone, Copy, Eq, PartialEq)]
    enum BootstrapMode {
        OuterCompleteTree,
        OuterCompleteTreeLegacy,
        NestedJob,
    }

    impl BootstrapMode {
        const fn label(self) -> &'static str {
            match self {
                Self::OuterCompleteTree => "outer_complete_tree",
                Self::OuterCompleteTreeLegacy => "outer_complete_tree_legacy",
                Self::NestedJob => "nested_job",
            }
        }

        const fn cpu_rate(self) -> u32 {
            match self {
                Self::OuterCompleteTree | Self::OuterCompleteTreeLegacy => {
                    SYNAPSE_OWNED_TREE_CPU_RATE
                }
                Self::NestedJob => NESTED_JOB_CPU_RATE,
            }
        }

        const fn role(self) -> &'static str {
            match self {
                Self::OuterCompleteTree => "broker_outer",
                Self::OuterCompleteTreeLegacy => "probe_outer",
                Self::NestedJob => "generation_inner",
            }
        }

        const fn expected_inherited_job(self) -> &'static str {
            match self {
                Self::OuterCompleteTree | Self::OuterCompleteTreeLegacy => OUTER_INHERITED_JOB,
                Self::NestedJob => INNER_INHERITED_JOB,
            }
        }

        const fn expected_inherited_job_name_template(self) -> &'static str {
            match self {
                Self::OuterCompleteTree | Self::OuterCompleteTreeLegacy => "",
                Self::NestedJob => INNER_INHERITED_JOB_NAME_TEMPLATE,
            }
        }

        const fn script_leaf_prefix(self) -> &'static str {
            match self {
                Self::OuterCompleteTree => OUTER_SCRIPT_LEAF_PREFIX,
                Self::OuterCompleteTreeLegacy => LEGACY_SCRIPT_LEAF_PREFIX,
                Self::NestedJob => INNER_SCRIPT_LEAF_PREFIX,
            }
        }

        const fn created_job_name_template(self) -> &'static str {
            match self {
                Self::OuterCompleteTreeLegacy => LEGACY_JOB_NAME_TEMPLATE,
                Self::OuterCompleteTree | Self::NestedJob => CAPABILITY_JOB_NAME_TEMPLATE,
            }
        }
    }

    struct BootstrapArgs {
        mode: BootstrapMode,
        launch_capability_argv: Option<String>,
        powershell: PathBuf,
        supervisor_script: PathBuf,
        working_directory: PathBuf,
        log_path: PathBuf,
    }

    struct FileLease {
        handle: ManuallyDrop<OwnedHandle>,
        final_dos_path: PathBuf,
        final_nt_path: OsString,
        volume_serial_u32: u32,
        file_id_128: String,
        sha256: String,
        length: u64,
        bytes: Option<Vec<u8>>,
    }

    struct FileHandleEvidence {
        final_dos_path: PathBuf,
        final_nt_path: OsString,
        volume_serial_u32: u32,
        file_id_128: String,
        sha256: String,
        length: u64,
        bytes: Option<Vec<u8>>,
    }

    #[derive(Deserialize, Serialize)]
    #[serde(deny_unknown_fields)]
    struct ExpectedFileIdentity {
        path: String,
        volume_serial_u32: u32,
        file_id_128: String,
        sha256: String,
        length: u64,
    }

    #[derive(Deserialize, Serialize)]
    #[serde(deny_unknown_fields)]
    struct ScriptBodyIdentity {
        sha256: String,
        length: u64,
    }

    #[derive(Deserialize, Serialize)]
    #[serde(deny_unknown_fields)]
    struct LaunchContract {
        schema: String,
        role: String,
        mode: String,
        bootstrap: ExpectedFileIdentity,
        powershell: ExpectedFileIdentity,
        script_body: ScriptBodyIdentity,
        broker_identity_sha256: String,
        launch_capability_sha256: String,
        outer_launch_capability_sha256: String,
        authority_epoch_i64: i64,
        created_job_name_template: String,
        expected_inherited_job: String,
        expected_inherited_job_name_template: String,
        expected_job_cpu_rate_u32: u32,
    }

    struct ValidatedLaunchContract {
        contract: LaunchContract,
        bootstrap: FileLease,
        bootstrap_mapped_nt_path: OsString,
        powershell: FileLease,
        script: FileLease,
    }

    #[derive(Clone)]
    struct ErrorPaths {
        working_directory: PathBuf,
        log_path: PathBuf,
    }

    struct ParseFailure {
        message: String,
        error_paths: Option<ErrorPaths>,
    }

    #[derive(Default)]
    struct BindingProgress {
        contract_bound: bool,
        mode_label: Option<String>,
        job_name: Option<String>,
        cpu_rate: Option<u32>,
    }

    impl BindingProgress {
        fn mark_contract_bound(&mut self, mode_label: &str, job_name: &str, cpu_rate: u32) {
            if self.contract_bound {
                return;
            }
            self.mode_label = Some(mode_label.to_owned());
            self.job_name = Some(job_name.to_owned());
            self.cpu_rate = Some(cpu_rate);
            self.contract_bound = true;
        }

        fn evidence(&self) -> String {
            if !self.contract_bound {
                return "contract_bound=false bound_mode=absent job_name=absent".to_owned();
            }
            format!(
                "contract_bound={} bound_mode={} job_name={} job_security_descriptor_sddl={JOB_SECURITY_DESCRIPTOR_SDDL} flags=0x{EXPECTED_JOB_LIMIT_FLAGS:08X} process_memory_limit_bytes={SYNAPSE_OWNED_TREE_HARD_LIMIT_BYTES} job_memory_limit_bytes={SYNAPSE_OWNED_TREE_HARD_LIMIT_BYTES} cpu_flags=0x{EXPECTED_CPU_RATE_FLAGS:08X} cpu_rate={}",
                self.contract_bound,
                self.mode_label.as_deref().unwrap_or("absent"),
                self.job_name.as_deref().unwrap_or("absent"),
                self.cpu_rate.map_or(0, |value| value)
            )
        }
    }

    impl BootstrapArgs {
        fn error_paths(&self) -> ErrorPaths {
            ErrorPaths {
                working_directory: self.working_directory.clone(),
                log_path: self.log_path.clone(),
            }
        }
    }

    fn parse_args() -> Result<BootstrapArgs, ParseFailure> {
        let values: Vec<OsString> = std::env::args_os().skip(1).collect();
        let (mode, launch_capability_argv, paths) = match values.as_slice() {
            [powershell, supervisor_script, working_directory, log_path] => (
                BootstrapMode::OuterCompleteTreeLegacy,
                None,
                [powershell, supervisor_script, working_directory, log_path],
            ),
            [
                marker,
                launch_capability,
                powershell,
                supervisor_script,
                working_directory,
                log_path,
            ] if marker.as_os_str() == OsStr::new("--outer-launch-capability") => {
                let launch_capability = launch_capability.to_str().ok_or_else(|| ParseFailure {
                    message: "SYNAPSE_SUPERVISOR_BOOTSTRAP_OUTER_CAPABILITY_NON_UNICODE remediation=regenerate the production broker task action with its uppercase SHA-256 capability"
                        .to_owned(),
                    error_paths: Some(ErrorPaths {
                        working_directory: PathBuf::from(working_directory),
                        log_path: PathBuf::from(log_path),
                    }),
                })?;
                if !is_uppercase_hex(launch_capability, 64) {
                    return Err(ParseFailure {
                        message: format!(
                            "SYNAPSE_SUPERVISOR_BOOTSTRAP_OUTER_CAPABILITY_INVALID expected=64_uppercase_hex actual_length={} remediation=regenerate the production broker task action",
                            launch_capability.len()
                        ),
                        error_paths: Some(ErrorPaths {
                            working_directory: PathBuf::from(working_directory),
                            log_path: PathBuf::from(log_path),
                        }),
                    });
                }
                (
                    BootstrapMode::OuterCompleteTree,
                    Some(launch_capability.to_owned()),
                    [powershell, supervisor_script, working_directory, log_path],
                )
            }
            [
                marker,
                powershell,
                supervisor_script,
                working_directory,
                log_path,
            ] if marker.as_os_str() == OsStr::new("--nested-job") => (
                BootstrapMode::NestedJob,
                None,
                [powershell, supervisor_script, working_directory, log_path],
            ),
            [_, _, _, working_directory, log_path] => {
                return Err(ParseFailure {
                    message: "SYNAPSE_SUPERVISOR_BOOTSTRAP_MODE_INVALID expected_first_marker=--nested-job actual_shape=5 remediation=regenerate the broker-owned nested bootstrap action"
                        .to_owned(),
                    error_paths: Some(ErrorPaths {
                        working_directory: PathBuf::from(working_directory),
                        log_path: PathBuf::from(log_path),
                    }),
                });
            }
            [_, _, _, _, working_directory, log_path] => {
                return Err(ParseFailure {
                    message: "SYNAPSE_SUPERVISOR_BOOTSTRAP_OUTER_MODE_INVALID expected_first_marker=--outer-launch-capability actual_shape=6 remediation=regenerate the production broker task action"
                        .to_owned(),
                    error_paths: Some(ErrorPaths {
                        working_directory: PathBuf::from(working_directory),
                        log_path: PathBuf::from(log_path),
                    }),
                });
            }
            _ => {
                return Err(ParseFailure {
                    message: format!(
                        "SYNAPSE_SUPERVISOR_BOOTSTRAP_ARGUMENTS_INVALID expected='4 legacy probe paths, --outer-launch-capability SHA256 followed by 4 paths, or --nested-job followed by 4 paths' actual={} remediation=regenerate the setup-owned Scheduled Task or broker action",
                        values.len()
                    ),
                    error_paths: None,
                });
            }
        };
        Ok(BootstrapArgs {
            mode,
            launch_capability_argv,
            powershell: PathBuf::from(paths[0]),
            supervisor_script: PathBuf::from(paths[1]),
            working_directory: PathBuf::from(paths[2]),
            log_path: PathBuf::from(paths[3]),
        })
    }

    fn validate_paths(arguments: &BootstrapArgs) -> Result<(), String> {
        if [
            arguments.powershell.as_os_str(),
            arguments.supervisor_script.as_os_str(),
            arguments.working_directory.as_os_str(),
            arguments.log_path.as_os_str(),
        ]
        .iter()
        .any(|value| value.to_str().is_none())
        {
            return Err(format!(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_NON_UNICODE_PATH mode={} remediation=install Synapse under Windows paths representable as Unicode; the bootstrap never lossily rewrites command-line bytes",
                arguments.mode.label()
            ));
        }
        if !arguments.powershell.is_absolute()
            || !arguments.supervisor_script.is_absolute()
            || !arguments.working_directory.is_absolute()
            || !arguments.log_path.is_absolute()
            || !arguments.working_directory.is_dir()
        {
            return Err(format!(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_PATH_INVALID mode={} remediation=all four setup-owned paths must be absolute and the working directory must exist; PowerShell and supervisor regular-file status is proved only through their retained exact-open handles",
                arguments.mode.label()
            ));
        }
        Ok(())
    }

    fn wide_null(value: &OsStr) -> Vec<u16> {
        value.encode_wide().chain(Some(0)).collect()
    }

    fn is_invalid_handle(handle: Handle) -> bool {
        handle == (-1isize as Handle)
    }

    fn uppercase_hex(bytes: &[u8]) -> String {
        let mut output = String::with_capacity(bytes.len() * 2);
        for byte in bytes {
            use std::fmt::Write as _;
            let _ = write!(output, "{byte:02X}");
        }
        output
    }

    fn is_uppercase_hex(value: &str, expected_length: usize) -> bool {
        value.len() == expected_length
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'A'..=b'F').contains(&byte))
    }

    fn validate_sha256(label: &str, value: &str) -> Result<(), String> {
        if !is_uppercase_hex(value, 64) {
            return Err(format!(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_CONTRACT_SHA256_INVALID field={label} expected=64_uppercase_hex actual_length={} remediation=regenerate the immutable launch contract",
                value.len()
            ));
        }
        Ok(())
    }

    fn validate_file_id_128(label: &str, value: &str) -> Result<(), String> {
        let bytes = value.as_bytes();
        let valid_hex = |slice: &[u8]| {
            slice
                .iter()
                .all(|byte| byte.is_ascii_digit() || (b'A'..=b'F').contains(byte))
        };
        let valid = bytes.len() == 49
            && bytes[16] == b':'
            && valid_hex(&bytes[..16])
            && valid_hex(&bytes[17..]);
        if !valid {
            return Err(format!(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_CONTRACT_FILE_ID_INVALID field={label} expected='16 uppercase hex volume identity, colon, 32 uppercase hex FILE_ID_128' actual_length={} remediation=regenerate the immutable launch contract from the native same-handle descriptor",
                value.len()
            ));
        }
        Ok(())
    }

    fn ordinal_compare_ignore_case(left: &OsStr, right: &OsStr) -> Result<Ordering, String> {
        let left: Vec<u16> = left.encode_wide().collect();
        let right: Vec<u16> = right.encode_wide().collect();
        let left_length = i32::try_from(left.len()).map_err(|error| {
            format!("SYNAPSE_SUPERVISOR_BOOTSTRAP_PATH_LENGTH_INVALID side=left error={error}")
        })?;
        let right_length = i32::try_from(right.len()).map_err(|error| {
            format!("SYNAPSE_SUPERVISOR_BOOTSTRAP_PATH_LENGTH_INVALID side=right error={error}")
        })?;
        // SAFETY: both slices remain live and their exact lengths are supplied.
        let comparison = unsafe {
            CompareStringOrdinal(
                left.as_ptr(),
                left_length,
                right.as_ptr(),
                right_length,
                TRUE,
            )
        };
        if comparison == 0 {
            return Err(last_error(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_ORDINAL_PATH_COMPARE_FAILED",
            ));
        }
        match comparison {
            1 => Ok(Ordering::Less),
            2 => Ok(Ordering::Equal),
            3 => Ok(Ordering::Greater),
            _ => Err(format!(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_ORDINAL_COMPARE_RESULT_INVALID actual={comparison}"
            )),
        }
    }

    fn ordinal_equals_ignore_case(left: &OsStr, right: &OsStr) -> Result<bool, String> {
        Ok(ordinal_compare_ignore_case(left, right)? == Ordering::Equal)
    }

    fn strip_extended_dos_prefix(path: OsString) -> Result<PathBuf, String> {
        let value = path.into_string().map_err(|_| {
            "SYNAPSE_SUPERVISOR_BOOTSTRAP_FINAL_PATH_NON_UNICODE remediation=install immutable launch artifacts under Unicode Windows paths"
                .to_owned()
        })?;
        if let Some(remainder) = value.strip_prefix(r"\\?\UNC\") {
            return Ok(PathBuf::from(format!(r"\\{remainder}")));
        }
        if let Some(remainder) = value.strip_prefix(r"\\?\") {
            return Ok(PathBuf::from(remainder));
        }
        Ok(PathBuf::from(value))
    }

    fn final_path_by_handle(file: Handle, flags: u32, label: &str) -> Result<OsString, String> {
        let mut capacity = 512usize;
        loop {
            if capacity > MAX_WINDOWS_PATH_UNITS + 1 {
                return Err(format!(
                    "SYNAPSE_SUPERVISOR_BOOTSTRAP_FINAL_PATH_TOO_LONG artifact={label} max_units={MAX_WINDOWS_PATH_UNITS}"
                ));
            }
            let mut buffer = vec![0u16; capacity];
            // SAFETY: the exact live file handle and writable UTF-16 buffer are valid.
            let length = unsafe {
                GetFinalPathNameByHandleW(
                    file,
                    buffer.as_mut_ptr(),
                    u32::try_from(buffer.len()).unwrap_or(u32::MAX),
                    flags,
                )
            };
            if length == 0 {
                return Err(format!(
                    "{} artifact={label}",
                    last_error("SYNAPSE_SUPERVISOR_BOOTSTRAP_FINAL_PATH_READBACK_FAILED")
                ));
            }
            let length = usize::try_from(length).unwrap_or(usize::MAX);
            if length < buffer.len() {
                buffer.truncate(length);
                return Ok(OsString::from_wide(&buffer));
            }
            capacity = length.saturating_add(1);
        }
    }

    fn current_module_path(module: Handle) -> Result<PathBuf, String> {
        let mut capacity = 512usize;
        loop {
            if capacity > MAX_WINDOWS_PATH_UNITS + 1 {
                return Err(format!(
                    "SYNAPSE_SUPERVISOR_BOOTSTRAP_MAIN_IMAGE_PATH_TOO_LONG max_units={MAX_WINDOWS_PATH_UNITS}"
                ));
            }
            let mut buffer = vec![0u16; capacity];
            // SAFETY: `module` is the current main module and the buffer is writable.
            let length = unsafe {
                GetModuleFileNameW(
                    module,
                    buffer.as_mut_ptr(),
                    u32::try_from(buffer.len()).unwrap_or(u32::MAX),
                )
            };
            if length == 0 {
                return Err(last_error(
                    "SYNAPSE_SUPERVISOR_BOOTSTRAP_MAIN_IMAGE_PATH_READBACK_FAILED",
                ));
            }
            let length = usize::try_from(length).unwrap_or(usize::MAX);
            if length < buffer.len() {
                buffer.truncate(length);
                return strip_extended_dos_prefix(OsString::from_wide(&buffer));
            }
            capacity = capacity.saturating_mul(2);
        }
    }

    fn mapped_image_path(process: Handle, module: Handle, label: &str) -> Result<OsString, String> {
        let mut capacity = 512usize;
        loop {
            if capacity > MAX_WINDOWS_PATH_UNITS + 1 {
                return Err(format!(
                    "SYNAPSE_SUPERVISOR_BOOTSTRAP_MAPPED_IMAGE_PATH_TOO_LONG artifact={label} max_units={MAX_WINDOWS_PATH_UNITS}"
                ));
            }
            let mut buffer = vec![0u16; capacity];
            // SAFETY: the pseudo process handle, mapped main-module address, and
            // writable buffer satisfy GetMappedFileNameW's contract.
            let length = unsafe {
                GetMappedFileNameW(
                    process,
                    module,
                    buffer.as_mut_ptr(),
                    u32::try_from(buffer.len()).unwrap_or(u32::MAX),
                )
            };
            if length == 0 {
                return Err(last_error(
                    "SYNAPSE_SUPERVISOR_BOOTSTRAP_MAPPED_IMAGE_PATH_READBACK_FAILED",
                ));
            }
            let length = usize::try_from(length).unwrap_or(usize::MAX);
            if length < buffer.len() {
                buffer.truncate(length);
                return Ok(OsString::from_wide(&buffer));
            }
            capacity = capacity.saturating_mul(2);
        }
    }

    fn file_time_value(value: FileTime) -> u64 {
        (u64::from(value.high_date_time) << 32) | u64::from(value.low_date_time)
    }

    fn process_creation_time(process: Handle, label: &str) -> Result<u64, String> {
        let mut creation = FileTime::default();
        let mut exit = FileTime::default();
        let mut kernel = FileTime::default();
        let mut user = FileTime::default();
        // SAFETY: the process handle and four exact FILETIME outputs are valid.
        if unsafe {
            GetProcessTimes(
                process,
                &raw mut creation,
                &raw mut exit,
                &raw mut kernel,
                &raw mut user,
            )
        } == FALSE
        {
            return Err(format!(
                "{} process_role={label}",
                last_error("SYNAPSE_SUPERVISOR_BOOTSTRAP_PROCESS_TIMES_READBACK_FAILED")
            ));
        }
        let creation = file_time_value(creation);
        if creation == 0 {
            return Err(format!(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_PROCESS_CREATION_TIME_INVALID process_role={label} creation_time=0"
            ));
        }
        Ok(creation)
    }

    fn thread_creation_time(thread: Handle, label: &str) -> Result<u64, String> {
        let mut creation = FileTime::default();
        let mut exit = FileTime::default();
        let mut kernel = FileTime::default();
        let mut user = FileTime::default();
        // SAFETY: the thread handle and four exact FILETIME outputs are valid.
        if unsafe {
            GetThreadTimes(
                thread,
                &raw mut creation,
                &raw mut exit,
                &raw mut kernel,
                &raw mut user,
            )
        } == FALSE
        {
            return Err(format!(
                "{} thread_role={label}",
                last_error("SYNAPSE_SUPERVISOR_BOOTSTRAP_THREAD_TIMES_READBACK_FAILED")
            ));
        }
        let creation = file_time_value(creation);
        if creation == 0 {
            return Err(format!(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_THREAD_CREATION_TIME_INVALID thread_role={label} creation_time=0"
            ));
        }
        Ok(creation)
    }

    fn query_full_process_image_path(process: Handle, label: &str) -> Result<PathBuf, String> {
        let mut buffer = vec![0u16; MAX_WINDOWS_PATH_UNITS + 1];
        let mut length = u32::try_from(buffer.len()).unwrap_or(u32::MAX);
        // SAFETY: the process handle and exact writable UTF-16 buffer are valid.
        if unsafe { QueryFullProcessImageNameW(process, 0, buffer.as_mut_ptr(), &raw mut length) }
            == FALSE
        {
            return Err(format!(
                "{} process_role={label}",
                last_error("SYNAPSE_SUPERVISOR_BOOTSTRAP_PROCESS_IMAGE_PATH_READBACK_FAILED")
            ));
        }
        let length = usize::try_from(length).unwrap_or(usize::MAX);
        if length == 0 || length >= buffer.len() {
            return Err(format!(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_PROCESS_IMAGE_PATH_LENGTH_INVALID process_role={label} actual={length} max={MAX_WINDOWS_PATH_UNITS}"
            ));
        }
        buffer.truncate(length);
        strip_extended_dos_prefix(OsString::from_wide(&buffer))
    }

    fn process_main_module(process: Handle, label: &str) -> Result<Handle, String> {
        let mut capacity = 32usize;
        loop {
            if capacity > MAX_PROCESS_MODULES {
                return Err(format!(
                    "SYNAPSE_SUPERVISOR_BOOTSTRAP_PROCESS_MODULE_COUNT_EXCEEDED process_role={label} max={MAX_PROCESS_MODULES}"
                ));
            }
            let mut modules = vec![null_mut(); capacity];
            let bytes = modules
                .len()
                .checked_mul(size_of::<Handle>())
                .and_then(|value| u32::try_from(value).ok())
                .ok_or_else(|| {
                    format!(
                        "SYNAPSE_SUPERVISOR_BOOTSTRAP_PROCESS_MODULE_BUFFER_OVERFLOW process_role={label}"
                    )
                })?;
            let mut needed = 0u32;
            // SAFETY: the process handle, writable module array, and exact byte
            // size satisfy EnumProcessModulesEx.
            if unsafe {
                EnumProcessModulesEx(
                    process,
                    modules.as_mut_ptr(),
                    bytes,
                    &raw mut needed,
                    LIST_MODULES_ALL,
                )
            } == FALSE
            {
                return Err(format!(
                    "{} process_role={label}",
                    last_error("SYNAPSE_SUPERVISOR_BOOTSTRAP_PROCESS_MODULE_ENUMERATION_FAILED")
                ));
            }
            let needed = usize::try_from(needed).unwrap_or(usize::MAX);
            if needed == 0 || needed % size_of::<Handle>() != 0 {
                return Err(format!(
                    "SYNAPSE_SUPERVISOR_BOOTSTRAP_PROCESS_MODULE_SIZE_INVALID process_role={label} needed_bytes={needed}"
                ));
            }
            if needed <= usize::try_from(bytes).unwrap_or(usize::MAX) {
                let main_module = modules[0];
                if main_module.is_null() {
                    return Err(format!(
                        "SYNAPSE_SUPERVISOR_BOOTSTRAP_PROCESS_MAIN_MODULE_ABSENT process_role={label}"
                    ));
                }
                return Ok(main_module);
            }
            capacity = needed
                .checked_add(size_of::<Handle>() - 1)
                .map(|value| value / size_of::<Handle>())
                .unwrap_or(usize::MAX);
        }
    }

    fn verify_same_file_lease(
        label: &str,
        expected: &FileLease,
        observed: &FileLease,
    ) -> Result<(), String> {
        if !ordinal_equals_ignore_case(
            expected.final_dos_path.as_os_str(),
            observed.final_dos_path.as_os_str(),
        )? || !ordinal_equals_ignore_case(&expected.final_nt_path, &observed.final_nt_path)?
            || expected.volume_serial_u32 != observed.volume_serial_u32
            || expected.file_id_128 != observed.file_id_128
            || expected.sha256 != observed.sha256
            || expected.length != observed.length
        {
            return Err(format!(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_PROCESS_IMAGE_IDENTITY_MISMATCH process_role={label} expected_path={} observed_path={} expected_nt={} observed_nt={} expected_volume_serial={} observed_volume_serial={} expected_file_id_128={} observed_file_id_128={} expected_sha256={} observed_sha256={} expected_length={} observed_length={} remediation=refuse IFEO, image substitution, PID reuse, or physical-file drift",
                expected.final_dos_path.display(),
                observed.final_dos_path.display(),
                expected.final_nt_path.to_string_lossy(),
                observed.final_nt_path.to_string_lossy(),
                expected.volume_serial_u32,
                observed.volume_serial_u32,
                expected.file_id_128,
                observed.file_id_128,
                expected.sha256,
                observed.sha256,
                expected.length,
                observed.length
            ));
        }
        Ok(())
    }

    fn attest_process_image(
        process: Handle,
        expected_process_id: u32,
        expected_lease: &FileLease,
        label: &str,
    ) -> Result<ProcessImageEvidence, String> {
        // SAFETY: the supplied process handle is live for this proof.
        let observed_process_id = unsafe { GetProcessId(process) };
        if observed_process_id == 0 {
            return Err(format!(
                "{} process_role={label}",
                last_error("SYNAPSE_SUPERVISOR_BOOTSTRAP_PROCESS_ID_READBACK_FAILED")
            ));
        }
        if observed_process_id != expected_process_id {
            return Err(format!(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_PROCESS_ID_MISMATCH process_role={label} expected={expected_process_id} actual={observed_process_id} remediation=refuse PID reuse or handle substitution"
            ));
        }
        let creation_time = process_creation_time(process, label)?;
        let full_dos_path = query_full_process_image_path(process, label)?;
        let main_module = process_main_module(process, label)?;
        let mapped_nt_path = mapped_image_path(process, main_module, label)?;
        let observed_lease =
            open_file_lease(&full_dos_path, false, &format!("{label}_process_image"))?;
        if !ordinal_equals_ignore_case(&mapped_nt_path, &observed_lease.final_nt_path)? {
            return Err(format!(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_PROCESS_MAPPED_IMAGE_MISMATCH process_role={label} mapped_nt={} final_nt={} full_path={} remediation=refuse IFEO or a process-image/path substitution",
                mapped_nt_path.to_string_lossy(),
                observed_lease.final_nt_path.to_string_lossy(),
                full_dos_path.display()
            ));
        }
        verify_same_file_lease(label, expected_lease, &observed_lease)?;
        Ok(ProcessImageEvidence {
            process_id: observed_process_id,
            creation_time,
            full_dos_path,
            mapped_nt_path,
        })
    }

    fn open_process_for_image_proof(process_id: u32, label: &str) -> Result<OwnedHandle, String> {
        // SAFETY: the PID is cross-bound by the caller; the handle is non-inheritable.
        let process = unsafe {
            OpenProcess(
                PROCESS_QUERY_INFORMATION | PROCESS_VM_READ,
                FALSE,
                process_id,
            )
        };
        if process.is_null() {
            return Err(format!(
                "{} process_role={label} process_id={process_id}",
                last_error("SYNAPSE_SUPERVISOR_BOOTSTRAP_PROCESS_OPEN_FAILED")
            ));
        }
        Ok(OwnedHandle(process))
    }

    fn process_parent_chain(process_id: u32) -> Result<(u32, u32), String> {
        // SAFETY: a system-wide read-only process snapshot is requested.
        let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
        if is_invalid_handle(snapshot) {
            return Err(last_error(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_PROCESS_SNAPSHOT_CREATE_FAILED",
            ));
        }
        let snapshot = OwnedHandle(snapshot);
        // SAFETY: zero is a valid initialization when `size` is then set.
        let mut entry: ProcessEntry32W = unsafe { zeroed() };
        entry.size = u32::try_from(size_of::<ProcessEntry32W>()).unwrap_or(u32::MAX);
        // SAFETY: snapshot and writable entry are exact Toolhelp inputs.
        if unsafe { Process32FirstW(snapshot.0, &raw mut entry) } == FALSE {
            return Err(last_error(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_PROCESS_SNAPSHOT_FIRST_FAILED",
            ));
        }
        let mut entries = Vec::new();
        loop {
            if entries.len() >= MAX_PROCESS_SNAPSHOT_ENTRIES {
                return Err(format!(
                    "SYNAPSE_SUPERVISOR_BOOTSTRAP_PROCESS_SNAPSHOT_BOUND_EXCEEDED max_snapshot_entries={MAX_PROCESS_SNAPSHOT_ENTRIES} remediation=refuse a truncated physical lineage inventory"
                ));
            }
            entries.push((entry.process_id, entry.parent_process_id));
            // SAFETY: snapshot and entry remain live; Process32NextW rewrites it.
            if unsafe { Process32NextW(snapshot.0, &raw mut entry) } == FALSE {
                // SAFETY: GetLastError immediately follows Process32NextW.
                let error = unsafe { GetLastError() };
                if error == ERROR_NO_MORE_FILES {
                    break;
                }
                return Err(explicit_win32_error(
                    "SYNAPSE_SUPERVISOR_BOOTSTRAP_PROCESS_SNAPSHOT_NEXT_FAILED",
                    error,
                ));
            }
        }
        entries.sort_unstable_by_key(|entry| entry.0);
        if entries.windows(2).any(|pair| pair[0].0 == pair[1].0) {
            return Err(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_PROCESS_SNAPSHOT_PID_DUPLICATE remediation=refuse an ambiguous process inventory"
                    .to_owned(),
            );
        }
        let parent_of = |child: u32| -> Result<u32, String> {
            let index = entries.binary_search_by_key(&child, |entry| entry.0).map_err(|_| {
                format!(
                    "SYNAPSE_SUPERVISOR_BOOTSTRAP_PROCESS_SNAPSHOT_PID_ABSENT process_id={child} max_snapshot_entries={MAX_PROCESS_SNAPSHOT_ENTRIES} remediation=refuse a missing or raced physical lineage"
                )
            })?;
            let parent = entries[index].1;
            if parent == 0 {
                return Err(format!(
                    "SYNAPSE_SUPERVISOR_BOOTSTRAP_PARENT_PROCESS_ID_ABSENT process_id={child}"
                ));
            }
            Ok(parent)
        };
        let parent_process_id = parent_of(process_id)?;
        let grandparent_process_id = parent_of(parent_process_id)?;
        Ok((parent_process_id, grandparent_process_id))
    }

    fn read_and_hash_file(
        file: Handle,
        length: u64,
        capture_bytes: bool,
        label: &str,
    ) -> Result<(String, Option<Vec<u8>>), String> {
        if length > MAX_ATTESTED_FILE_BYTES {
            return Err(format!(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_ARTIFACT_TOO_LARGE artifact={label} length={length} max={MAX_ATTESTED_FILE_BYTES} remediation=restore the immutable setup artifact"
            ));
        }
        let capacity = usize::try_from(length).map_err(|error| {
            format!(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_ARTIFACT_LENGTH_CONVERSION_FAILED artifact={label} length={length} error={error}"
            )
        })?;
        let mut captured = capture_bytes.then(|| Vec::with_capacity(capacity));
        let mut hasher = Sha256::new();
        let mut consumed = 0u64;
        let mut buffer = [0u8; 64 * 1024];
        while consumed < length {
            let remaining = usize::try_from(length - consumed).unwrap_or(usize::MAX);
            let request = remaining.min(buffer.len());
            let mut read = 0u32;
            // SAFETY: the file offset advances synchronously and the exact
            // writable prefix is supplied with no OVERLAPPED structure.
            if unsafe {
                ReadFile(
                    file,
                    buffer.as_mut_ptr().cast(),
                    u32::try_from(request).unwrap_or(u32::MAX),
                    &raw mut read,
                    null_mut(),
                )
            } == FALSE
            {
                return Err(format!(
                    "{} artifact={label} consumed={consumed} expected_length={length}",
                    last_error("SYNAPSE_SUPERVISOR_BOOTSTRAP_ARTIFACT_READ_FAILED")
                ));
            }
            if read == 0 {
                return Err(format!(
                    "SYNAPSE_SUPERVISOR_BOOTSTRAP_ARTIFACT_SHORT_READ artifact={label} consumed={consumed} expected_length={length} remediation=restore the immutable setup artifact"
                ));
            }
            let read = usize::try_from(read).unwrap_or(usize::MAX);
            hasher.update(&buffer[..read]);
            if let Some(bytes) = captured.as_mut() {
                bytes.extend_from_slice(&buffer[..read]);
            }
            consumed = consumed
                .checked_add(u64::try_from(read).unwrap_or(u64::MAX))
                .ok_or_else(|| {
                    format!("SYNAPSE_SUPERVISOR_BOOTSTRAP_ARTIFACT_READ_OVERFLOW artifact={label}")
                })?;
        }
        let mut trailing = 0u8;
        let mut trailing_read = 0u32;
        // SAFETY: this exact one-byte read proves EOF after the snapshotted length.
        if unsafe {
            ReadFile(
                file,
                (&raw mut trailing).cast(),
                1,
                &raw mut trailing_read,
                null_mut(),
            )
        } == FALSE
        {
            return Err(format!(
                "{} artifact={label} stage=eof_readback",
                last_error("SYNAPSE_SUPERVISOR_BOOTSTRAP_ARTIFACT_READ_FAILED")
            ));
        }
        if trailing_read != 0 {
            return Err(format!(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_ARTIFACT_GREW_DURING_READ artifact={label} expected_length={length} remediation=restore the immutable setup artifact"
            ));
        }
        Ok((uppercase_hex(&hasher.finalize()), captured))
    }

    fn attest_file_handle(
        handle: Handle,
        requested_path: Option<&Path>,
        capture_bytes: bool,
        label: &str,
    ) -> Result<FileHandleEvidence, String> {
        if handle.is_null() || is_invalid_handle(handle) {
            return Err(format!(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_ARTIFACT_HANDLE_INVALID artifact={label} remediation=refuse a null or invalid exact file handle"
            ));
        }
        // SAFETY: this exact regular-file handle is owned by the caller. Resetting
        // its synchronous file pointer makes the subsequent length-bounded hash
        // independent of any undocumented initial position on debug-event hFile.
        if unsafe { SetFilePointerEx(handle, 0, null_mut(), FILE_BEGIN) } == FALSE {
            return Err(format!(
                "{} artifact={label}",
                last_error("SYNAPSE_SUPERVISOR_BOOTSTRAP_ARTIFACT_REWIND_FAILED")
            ));
        }
        let mut before = ByHandleFileInformation::default();
        // SAFETY: the exact live file handle and writable structure are valid.
        if unsafe { GetFileInformationByHandle(handle, &raw mut before) } == FALSE {
            return Err(format!(
                "{} artifact={label}",
                last_error("SYNAPSE_SUPERVISOR_BOOTSTRAP_ARTIFACT_INFORMATION_FAILED")
            ));
        }
        if before.file_attributes & (FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_REPARSE_POINT) != 0 {
            return Err(format!(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_ARTIFACT_TYPE_INVALID artifact={label} attributes=0x{:08X} remediation=restore a regular non-reparse immutable file",
                before.file_attributes
            ));
        }
        let mut file_id = FileIdInfo::default();
        // SAFETY: FileIdInfo is the exact class-18 writable ABI.
        if unsafe {
            GetFileInformationByHandleEx(
                handle,
                FILE_ID_INFO_CLASS,
                (&raw mut file_id).cast(),
                u32::try_from(size_of::<FileIdInfo>()).unwrap_or(u32::MAX),
            )
        } == FALSE
        {
            return Err(format!(
                "{} artifact={label}",
                last_error("SYNAPSE_SUPERVISOR_BOOTSTRAP_ARTIFACT_FILE_ID_FAILED")
            ));
        }
        let mut volume_serial_u32 = 0u32;
        // SAFETY: only the documented serial-number output is requested.
        if unsafe {
            GetVolumeInformationByHandleW(
                handle,
                null_mut(),
                0,
                &raw mut volume_serial_u32,
                null_mut(),
                null_mut(),
                null_mut(),
                0,
            )
        } == FALSE
        {
            return Err(format!(
                "{} artifact={label}",
                last_error("SYNAPSE_SUPERVISOR_BOOTSTRAP_ARTIFACT_VOLUME_SERIAL_FAILED")
            ));
        }
        if before.volume_serial_number != volume_serial_u32 {
            return Err(format!(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_ARTIFACT_VOLUME_SERIAL_DRIFT artifact={label} by_handle={} volume_api={} remediation=refuse a physically ambiguous artifact",
                before.volume_serial_number, volume_serial_u32
            ));
        }
        let final_dos_path = strip_extended_dos_prefix(final_path_by_handle(handle, 0, label)?)?;
        if let Some(path) = requested_path
            && !ordinal_equals_ignore_case(path.as_os_str(), final_dos_path.as_os_str())?
        {
            return Err(format!(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_ARTIFACT_FINAL_PATH_MISMATCH artifact={label} requested={} final={} remediation=launch only the canonical non-aliased immutable path",
                path.display(),
                final_dos_path.display()
            ));
        }
        let final_nt_path = final_path_by_handle(handle, VOLUME_NAME_NT, label)?;
        let mut signed_length = 0i64;
        // SAFETY: the exact live file handle and writable length are valid.
        if unsafe { GetFileSizeEx(handle, &raw mut signed_length) } == FALSE || signed_length < 0 {
            return Err(format!(
                "{} artifact={label} signed_length={signed_length}",
                last_error("SYNAPSE_SUPERVISOR_BOOTSTRAP_ARTIFACT_LENGTH_FAILED")
            ));
        }
        let length = u64::try_from(signed_length).map_err(|error| {
            format!(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_ARTIFACT_LENGTH_CONVERSION_FAILED artifact={label} signed_length={signed_length} error={error}"
            )
        })?;
        let by_handle_length =
            (u64::from(before.file_size_high) << 32) | u64::from(before.file_size_low);
        if by_handle_length != length {
            return Err(format!(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_ARTIFACT_LENGTH_READBACK_MISMATCH artifact={label} get_file_size_ex={length} by_handle={by_handle_length} remediation=refuse a physically ambiguous artifact"
            ));
        }
        let (sha256, bytes) = read_and_hash_file(handle, length, capture_bytes, label)?;
        let mut after = ByHandleFileInformation::default();
        let mut file_id_after = FileIdInfo::default();
        // SAFETY: both exact post-read metadata buffers are writable.
        if unsafe { GetFileInformationByHandle(handle, &raw mut after) } == FALSE
            || unsafe {
                GetFileInformationByHandleEx(
                    handle,
                    FILE_ID_INFO_CLASS,
                    (&raw mut file_id_after).cast(),
                    u32::try_from(size_of::<FileIdInfo>()).unwrap_or(u32::MAX),
                )
            } == FALSE
        {
            return Err(format!(
                "{} artifact={label} stage=post_hash",
                last_error("SYNAPSE_SUPERVISOR_BOOTSTRAP_ARTIFACT_INFORMATION_FAILED")
            ));
        }
        if before.file_attributes != after.file_attributes
            || before.creation_time != after.creation_time
            || before.last_write_time != after.last_write_time
            || before.volume_serial_number != after.volume_serial_number
            || before.file_size_high != after.file_size_high
            || before.file_size_low != after.file_size_low
            || before.number_of_links != after.number_of_links
            || before.file_index_high != after.file_index_high
            || before.file_index_low != after.file_index_low
            || file_id != file_id_after
        {
            return Err(format!(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_ARTIFACT_CHANGED_DURING_ATTESTATION artifact={label} remediation=restore the immutable setup artifact and retry"
            ));
        }
        let file_id_128 = format!(
            "{:016X}:{}",
            file_id.volume_serial_number,
            uppercase_hex(&file_id.file_id)
        );
        Ok(FileHandleEvidence {
            final_dos_path,
            final_nt_path,
            volume_serial_u32,
            file_id_128,
            sha256,
            length,
            bytes,
        })
    }

    fn open_file_lease(path: &Path, capture_bytes: bool, label: &str) -> Result<FileLease, String> {
        if !path.is_absolute() || path.as_os_str().to_str().is_none() {
            return Err(format!(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_ARTIFACT_PATH_INVALID artifact={label} path={} requirement=absolute_unicode_final_dos_path",
                path.display()
            ));
        }
        let wide_path = wide_null(path.as_os_str());
        // SAFETY: the terminated path is live. FILE_SHARE_READ is deliberately
        // the only share permission, excluding pre-existing/new writers and
        // delete/rename leases while this handle remains live.
        let raw_file = unsafe {
            CreateFileW(
                wide_path.as_ptr(),
                GENERIC_READ,
                FILE_SHARE_READ,
                null(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OPEN_REPARSE_POINT,
                null_mut(),
            )
        };
        if is_invalid_handle(raw_file) {
            return Err(format!(
                "{} artifact={label} path={}",
                last_error("SYNAPSE_SUPERVISOR_BOOTSTRAP_ARTIFACT_EXACT_OPEN_FAILED"),
                path.display()
            ));
        }
        let handle = OwnedHandle(raw_file);
        let evidence = attest_file_handle(handle.0, Some(path), capture_bytes, label)?;
        Ok(FileLease {
            // Like the KILL_ON_JOB_CLOSE Job handle, every successfully
            // established artifact lease is retained until process teardown.
            // This covers post-resume logging/wait failures as well as the
            // normal child-exit path without an early-drop window.
            handle: ManuallyDrop::new(handle),
            final_dos_path: evidence.final_dos_path,
            final_nt_path: evidence.final_nt_path,
            volume_serial_u32: evidence.volume_serial_u32,
            file_id_128: evidence.file_id_128,
            sha256: evidence.sha256,
            length: evidence.length,
            bytes: evidence.bytes,
        })
    }

    fn verify_expected_file_identity(
        label: &str,
        expected: &ExpectedFileIdentity,
        actual: &FileLease,
    ) -> Result<(), String> {
        validate_file_id_128(&format!("{label}.file_id_128"), &expected.file_id_128)?;
        validate_sha256(&format!("{label}.sha256"), &expected.sha256)?;
        let expected_path = Path::new(&expected.path);
        if !expected_path.is_absolute()
            || !ordinal_equals_ignore_case(
                expected_path.as_os_str(),
                actual.final_dos_path.as_os_str(),
            )?
            || expected.volume_serial_u32 != actual.volume_serial_u32
            || expected.file_id_128 != actual.file_id_128
            || expected.sha256 != actual.sha256
            || expected.length != actual.length
        {
            return Err(format!(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_CONTRACT_ARTIFACT_MISMATCH artifact={label} expected_path={} actual_path={} expected_volume_serial={} actual_volume_serial={} expected_file_id_128={} actual_file_id_128={} expected_sha256={} actual_sha256={} expected_length={} actual_length={} remediation=refuse a launch whose signed header does not describe the retained same-handle artifact",
                expected.path,
                actual.final_dos_path.display(),
                expected.volume_serial_u32,
                actual.volume_serial_u32,
                expected.file_id_128,
                actual.file_id_128,
                expected.sha256,
                actual.sha256,
                expected.length,
                actual.length
            ));
        }
        Ok(())
    }

    fn verify_content_addressed_leaf(
        label: &str,
        path: &Path,
        prefix: &str,
        sha256: &str,
        extension: &str,
    ) -> Result<(), String> {
        let actual = path.file_name().and_then(OsStr::to_str).ok_or_else(|| {
            format!(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_CONTENT_ADDRESS_LEAF_INVALID artifact={label} path={} remediation=restore the immutable content-addressed artifact",
                path.display()
            )
        })?;
        let expected = format!("{prefix}{sha256}{extension}");
        if actual != expected {
            return Err(format!(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_CONTENT_ADDRESS_LEAF_MISMATCH artifact={label} expected={expected} actual={actual} remediation=refuse path/content substitution or non-uppercase content-addressing"
            ));
        }
        Ok(())
    }

    fn attest_current_image() -> Result<(FileLease, OsString), String> {
        // SAFETY: a null module name returns the calling process's main image.
        let module = unsafe { GetModuleHandleW(null()) };
        if module.is_null() {
            return Err(last_error(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_MAIN_MODULE_READBACK_FAILED",
            ));
        }
        let loader_path = current_module_path(module)?;
        // SAFETY: the pseudo current-process handle is always valid.
        let current_process = unsafe { GetCurrentProcess() };
        let mapped_path_before =
            mapped_image_path(current_process, module, "bootstrap_before_lease")?;
        // The path-derived exact-open uses GENERIC_READ with FILE_SHARE_READ
        // only. Reading the mapped namespace again after that lease brackets a
        // same-user rename-back race: both mapped observations and the
        // same-handle final NT name must identify one immutable image.
        let lease = open_file_lease(&loader_path, false, "bootstrap")?;
        let mapped_path_after =
            mapped_image_path(current_process, module, "bootstrap_after_lease")?;
        if !ordinal_equals_ignore_case(&mapped_path_before, &mapped_path_after)?
            || !ordinal_equals_ignore_case(&lease.final_nt_path, &mapped_path_before)?
            || !ordinal_equals_ignore_case(&lease.final_nt_path, &mapped_path_after)?
        {
            return Err(format!(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_MAPPED_IMAGE_MISMATCH loader_path={} final_path={} final_nt_path={} mapped_nt_before={} mapped_nt_after={} proof=M1_then_FILE_SHARE_READ_only_lease_then_M2 requirement=M1_equals_M2_equals_same_handle_final_NT nonhostile_same_user_required remediation=refuse a main-image/path substitution",
                loader_path.display(),
                lease.final_dos_path.display(),
                lease.final_nt_path.to_string_lossy(),
                mapped_path_before.to_string_lossy(),
                mapped_path_after.to_string_lossy()
            ));
        }
        verify_content_addressed_leaf(
            "bootstrap",
            &lease.final_dos_path,
            BOOTSTRAP_LEAF_PREFIX,
            &lease.sha256,
            ".exe",
        )?;
        Ok((lease, mapped_path_after))
    }

    fn parse_and_verify_contract(
        mode: BootstrapMode,
        bootstrap: &FileLease,
        powershell: &FileLease,
        script: &FileLease,
    ) -> Result<LaunchContract, String> {
        let bytes = script.bytes.as_deref().ok_or_else(|| {
            "SYNAPSE_SUPERVISOR_BOOTSTRAP_CONTRACT_SCRIPT_BYTES_ABSENT remediation=the retained script must be captured exactly once during attestation"
                .to_owned()
        })?;
        std::str::from_utf8(bytes).map_err(|error| {
            format!(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_CONTRACT_SCRIPT_UTF8_INVALID error={error} remediation=regenerate the immutable supervisor as strict UTF-8"
            )
        })?;
        let first_lf = bytes.iter().position(|byte| *byte == b'\n').ok_or_else(|| {
            "SYNAPSE_SUPERVISOR_BOOTSTRAP_CONTRACT_HEADER_MISSING_LF remediation=the V2 header must be the complete first UTF-8 line"
                .to_owned()
        })?;
        if first_lf > MAX_LAUNCH_CONTRACT_HEADER_BYTES {
            return Err(format!(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_CONTRACT_HEADER_TOO_LARGE actual={first_lf} max={MAX_LAUNCH_CONTRACT_HEADER_BYTES}"
            ));
        }
        if !bytes.starts_with(&UTF8_BOM) {
            return Err(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_CONTRACT_UTF8_BOM_ABSENT requirement=exact_EF_BB_BF_at_byte_zero remediation=regenerate the WinPS-5.1-safe immutable supervisor"
                    .to_owned(),
            );
        }
        if bytes[UTF8_BOM.len()..].starts_with(&UTF8_BOM) {
            return Err(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_CONTRACT_UTF8_BOM_DUPLICATE requirement=exactly_one_EF_BB_BF remediation=regenerate the immutable supervisor"
                    .to_owned(),
            );
        }
        let mut header_bytes = &bytes[..first_lf];
        if header_bytes.last() == Some(&b'\r') {
            header_bytes = &header_bytes[..header_bytes.len() - 1];
        }
        let header_bytes = header_bytes.strip_prefix(&UTF8_BOM).ok_or_else(|| {
            "SYNAPSE_SUPERVISOR_BOOTSTRAP_CONTRACT_UTF8_BOM_POSITION_INVALID requirement=exact_EF_BB_BF_before_header_marker"
                .to_owned()
        })?;
        let header = std::str::from_utf8(header_bytes).map_err(|error| {
            format!("SYNAPSE_SUPERVISOR_BOOTSTRAP_CONTRACT_HEADER_UTF8_INVALID error={error}")
        })?;
        let canonical_json = header.strip_prefix(LAUNCH_CONTRACT_PREFIX).ok_or_else(|| {
            format!(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_CONTRACT_HEADER_PREFIX_INVALID expected={LAUNCH_CONTRACT_PREFIX:?} remediation=regenerate the immutable supervisor"
            )
        })?;
        let contract: LaunchContract = serde_json::from_str(canonical_json).map_err(|error| {
            format!(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_CONTRACT_JSON_INVALID error={error} remediation=regenerate the canonical V2 header"
            )
        })?;
        let serialized = serde_json::to_string(&contract).map_err(|error| {
            format!("SYNAPSE_SUPERVISOR_BOOTSTRAP_CONTRACT_JSON_SERIALIZE_FAILED error={error}")
        })?;
        if serialized != canonical_json {
            return Err(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_CONTRACT_JSON_NON_CANONICAL requirement=exact_declared_key_order_no_whitespace_minimal_json remediation=regenerate the canonical V2 header"
                    .to_owned(),
            );
        }
        if contract.schema != LAUNCH_CONTRACT_SCHEMA
            || contract.role != mode.role()
            || contract.mode != mode.label()
            || contract.created_job_name_template != mode.created_job_name_template()
            || contract.expected_inherited_job != mode.expected_inherited_job()
            || contract.expected_inherited_job_name_template
                != mode.expected_inherited_job_name_template()
            || contract.expected_job_cpu_rate_u32 != mode.cpu_rate()
        {
            return Err(format!(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_CONTRACT_SHAPE_MISMATCH expected_schema={LAUNCH_CONTRACT_SCHEMA} actual_schema={} expected_role={} actual_role={} expected_mode={} actual_mode={} expected_created_job_name_template={} actual_created_job_name_template={} expected_inherited_job={} actual_inherited_job={} expected_inherited_job_name_template={} actual_inherited_job_name_template={} expected_cpu_rate={} actual_cpu_rate={} remediation=regenerate the exact role-specific V2 launch contract",
                contract.schema,
                mode.role(),
                contract.role,
                mode.label(),
                contract.mode,
                mode.created_job_name_template(),
                contract.created_job_name_template,
                mode.expected_inherited_job(),
                contract.expected_inherited_job,
                mode.expected_inherited_job_name_template(),
                contract.expected_inherited_job_name_template,
                mode.cpu_rate(),
                contract.expected_job_cpu_rate_u32
            ));
        }
        let epoch_valid = match mode {
            BootstrapMode::OuterCompleteTree | BootstrapMode::OuterCompleteTreeLegacy => {
                contract.authority_epoch_i64 == 0
            }
            BootstrapMode::NestedJob => contract.authority_epoch_i64 > 0,
        };
        if !epoch_valid {
            return Err(format!(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_CONTRACT_EPOCH_INVALID mode={} authority_epoch_i64={} requirement='outer=0; inner=>0' remediation=regenerate the role-specific launch contract",
                mode.label(),
                contract.authority_epoch_i64
            ));
        }
        validate_sha256("broker_identity_sha256", &contract.broker_identity_sha256)?;
        validate_sha256(
            "launch_capability_sha256",
            &contract.launch_capability_sha256,
        )?;
        validate_sha256(
            "outer_launch_capability_sha256",
            &contract.outer_launch_capability_sha256,
        )?;
        if contract.broker_identity_sha256 == contract.launch_capability_sha256 {
            return Err(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_CONTRACT_CAPABILITY_DOMAIN_COLLISION broker_identity_sha256_equals_launch_capability_sha256 remediation=regenerate the nonce-derived task or generation launch capability independently of the semantic broker identity"
                    .to_owned(),
            );
        }
        match mode {
            BootstrapMode::OuterCompleteTree | BootstrapMode::OuterCompleteTreeLegacy => {
                if contract.outer_launch_capability_sha256 != contract.launch_capability_sha256 {
                    return Err(format!(
                        "SYNAPSE_SUPERVISOR_BOOTSTRAP_CONTRACT_OUTER_CAPABILITY_MISMATCH mode={} launch_capability_sha256={} outer_launch_capability_sha256={} remediation=outer/probe contracts must self-bind one launch capability",
                        mode.label(),
                        contract.launch_capability_sha256,
                        contract.outer_launch_capability_sha256
                    ));
                }
            }
            BootstrapMode::NestedJob => {
                if contract.broker_identity_sha256 == contract.outer_launch_capability_sha256 {
                    return Err(
                        "SYNAPSE_SUPERVISOR_BOOTSTRAP_CONTRACT_BROKER_OUTER_CAPABILITY_COLLISION remediation=regenerate the semantic broker identity and permanent outer launch capability in independent domains"
                            .to_owned(),
                    );
                }
                if contract.outer_launch_capability_sha256 == contract.launch_capability_sha256 {
                    return Err(
                        "SYNAPSE_SUPERVISOR_BOOTSTRAP_CONTRACT_INNER_OUTER_CAPABILITY_COLLISION remediation=regenerate independent broker-task and generation launch capabilities"
                            .to_owned(),
                    );
                }
            }
        }
        verify_expected_file_identity("bootstrap", &contract.bootstrap, bootstrap)?;
        verify_expected_file_identity("powershell", &contract.powershell, powershell)?;

        let body = &bytes[first_lf + 1..];
        let body_length = u64::try_from(body.len()).map_err(|error| {
            format!(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_CONTRACT_BODY_LENGTH_CONVERSION_FAILED error={error}"
            )
        })?;
        let body_sha256 = uppercase_hex(&Sha256::digest(body));
        validate_sha256("script_body.sha256", &contract.script_body.sha256)?;
        if body.is_empty()
            || contract.script_body.length != body_length
            || contract.script_body.sha256 != body_sha256
        {
            return Err(format!(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_CONTRACT_BODY_MISMATCH expected_sha256={} actual_sha256={body_sha256} expected_length={} actual_length={body_length} nonempty={} remediation=restore the exact immutable supervisor body",
                contract.script_body.sha256,
                contract.script_body.length,
                !body.is_empty()
            ));
        }
        verify_content_addressed_leaf(
            "script",
            &script.final_dos_path,
            mode.script_leaf_prefix(),
            &script.sha256,
            ".ps1",
        )?;
        Ok(contract)
    }

    fn validate_launch_contract(
        arguments: &BootstrapArgs,
    ) -> Result<ValidatedLaunchContract, String> {
        let (bootstrap, bootstrap_mapped_nt_path) = attest_current_image()?;
        let powershell = open_file_lease(&arguments.powershell, false, "powershell")?;
        let script = open_file_lease(&arguments.supervisor_script, true, "script")?;
        let contract = parse_and_verify_contract(arguments.mode, &bootstrap, &powershell, &script)?;
        match (arguments.mode, arguments.launch_capability_argv.as_deref()) {
            (BootstrapMode::OuterCompleteTree, Some(argument_capability))
                if argument_capability == contract.launch_capability_sha256 => {}
            (BootstrapMode::OuterCompleteTree, actual) => {
                return Err(format!(
                    "SYNAPSE_SUPERVISOR_BOOTSTRAP_OUTER_CAPABILITY_ARGV_CONTRACT_MISMATCH argv={} contract={} remediation=the immutable task action and V2 header must bind the same production launch capability",
                    actual.unwrap_or("absent"),
                    contract.launch_capability_sha256
                ));
            }
            (BootstrapMode::OuterCompleteTreeLegacy | BootstrapMode::NestedJob, None) => {}
            (mode, Some(actual)) => {
                return Err(format!(
                    "SYNAPSE_SUPERVISOR_BOOTSTRAP_CAPABILITY_ARGV_UNEXPECTED mode={} actual={actual}",
                    mode.label()
                ));
            }
        }
        Ok(ValidatedLaunchContract {
            contract,
            bootstrap,
            bootstrap_mapped_nt_path,
            powershell,
            script,
        })
    }

    fn is_contract_environment_key(key: &OsStr) -> Result<bool, String> {
        for expected in CONTRACT_ENV_KEYS
            .iter()
            .copied()
            .chain(std::iter::once(EXPECTED_RATE_ENV_KEY))
        {
            if ordinal_compare_ignore_case(key, OsStr::new(expected))? == Ordering::Equal {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn sort_and_reject_duplicate_environment_keys(
        entries: &mut [(OsString, OsString)],
    ) -> Result<(), String> {
        // A fallible insertion sort keeps every comparison on the documented
        // Windows Unicode-ordinal, case-insensitive surface. Environment blocks
        // are small, and this avoids ASCII folding or lossy UTF-16 conversion.
        for index in 1..entries.len() {
            let mut cursor = index;
            while cursor > 0 {
                match ordinal_compare_ignore_case(
                    entries[cursor - 1].0.as_os_str(),
                    entries[cursor].0.as_os_str(),
                )? {
                    Ordering::Greater => entries.swap(cursor - 1, cursor),
                    Ordering::Equal => {
                        return Err(format!(
                            "SYNAPSE_SUPERVISOR_BOOTSTRAP_ENVIRONMENT_KEY_DUPLICATE first={} second={} comparison=CompareStringOrdinal_ignore_case remediation=remove the ambiguous inherited environment entry",
                            entries[cursor - 1].0.to_string_lossy(),
                            entries[cursor].0.to_string_lossy()
                        ));
                    }
                    Ordering::Less => break,
                }
                cursor -= 1;
            }
        }
        Ok(())
    }

    fn build_child_environment(
        mode: BootstrapMode,
        validated: &ValidatedLaunchContract,
        created_job_name: &str,
        expected_inherited_job_name: &str,
    ) -> Result<Vec<u16>, String> {
        let contract = &validated.contract;
        let bootstrap_path = validated.bootstrap.final_dos_path.to_str().ok_or_else(|| {
            "SYNAPSE_SUPERVISOR_BOOTSTRAP_ENVIRONMENT_PATH_NON_UNICODE artifact=bootstrap"
                .to_owned()
        })?;
        let powershell_path = validated
            .powershell
            .final_dos_path
            .to_str()
            .ok_or_else(|| {
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_ENVIRONMENT_PATH_NON_UNICODE artifact=powershell"
                    .to_owned()
            })?;
        let script_path = validated.script.final_dos_path.to_str().ok_or_else(|| {
            "SYNAPSE_SUPERVISOR_BOOTSTRAP_ENVIRONMENT_PATH_NON_UNICODE artifact=script".to_owned()
        })?;
        let mut entries: Vec<(OsString, OsString)> = Vec::new();
        for (key, value) in std::env::vars_os() {
            if !is_contract_environment_key(key.as_os_str())? {
                entries.push((key, value));
            }
        }
        let mut push = |key: &str, value: String| {
            entries.push((OsString::from(key), OsString::from(value)));
        };
        push(
            "SYNAPSE_LAUNCH_CONTRACT_SCHEMA",
            LAUNCH_CONTRACT_SCHEMA.to_owned(),
        );
        push("SYNAPSE_LAUNCH_ROLE", mode.role().to_owned());
        push("SYNAPSE_LAUNCH_MODE", mode.label().to_owned());
        push("SYNAPSE_BOOTSTRAP_PATH", bootstrap_path.to_owned());
        push(
            "SYNAPSE_BOOTSTRAP_VOLUME_SERIAL",
            validated.bootstrap.volume_serial_u32.to_string(),
        );
        push(
            "SYNAPSE_BOOTSTRAP_FILE_ID_128",
            validated.bootstrap.file_id_128.clone(),
        );
        push(
            "SYNAPSE_BOOTSTRAP_SHA256",
            validated.bootstrap.sha256.clone(),
        );
        push(
            "SYNAPSE_BOOTSTRAP_LENGTH",
            validated.bootstrap.length.to_string(),
        );
        push("SYNAPSE_POWERSHELL_PATH", powershell_path.to_owned());
        push(
            "SYNAPSE_POWERSHELL_VOLUME_SERIAL",
            validated.powershell.volume_serial_u32.to_string(),
        );
        push(
            "SYNAPSE_POWERSHELL_FILE_ID_128",
            validated.powershell.file_id_128.clone(),
        );
        push(
            "SYNAPSE_POWERSHELL_SHA256",
            validated.powershell.sha256.clone(),
        );
        push(
            "SYNAPSE_POWERSHELL_LENGTH",
            validated.powershell.length.to_string(),
        );
        push("SYNAPSE_SCRIPT_PATH", script_path.to_owned());
        push(
            "SYNAPSE_SCRIPT_VOLUME_SERIAL",
            validated.script.volume_serial_u32.to_string(),
        );
        push(
            "SYNAPSE_SCRIPT_FILE_ID_128",
            validated.script.file_id_128.clone(),
        );
        push("SYNAPSE_SCRIPT_SHA256", validated.script.sha256.clone());
        push("SYNAPSE_SCRIPT_LENGTH", validated.script.length.to_string());
        push(
            "SYNAPSE_BROKER_IDENTITY_SHA256",
            contract.broker_identity_sha256.clone(),
        );
        push(
            "SYNAPSE_LAUNCH_CAPABILITY_SHA256",
            contract.launch_capability_sha256.clone(),
        );
        push(
            "SYNAPSE_OUTER_LAUNCH_CAPABILITY_SHA256",
            contract.outer_launch_capability_sha256.clone(),
        );
        push(
            "SYNAPSE_AUTHORITY_EPOCH",
            contract.authority_epoch_i64.to_string(),
        );
        push("SYNAPSE_CREATED_JOB_NAME", created_job_name.to_owned());
        push(
            "SYNAPSE_EXPECTED_INHERITED_JOB",
            mode.expected_inherited_job().to_owned(),
        );
        push(
            "SYNAPSE_EXPECTED_INHERITED_JOB_NAME",
            expected_inherited_job_name.to_owned(),
        );
        push(EXPECTED_RATE_ENV_KEY, mode.cpu_rate().to_string());
        sort_and_reject_duplicate_environment_keys(&mut entries)?;
        let mut block = Vec::new();
        for (key, value) in entries {
            let key_units: Vec<u16> = key.encode_wide().collect();
            let value_units: Vec<u16> = value.encode_wide().collect();
            if key_units.contains(&0) || value_units.contains(&0) {
                return Err(
                    "SYNAPSE_SUPERVISOR_BOOTSTRAP_ENVIRONMENT_NUL_INVALID remediation=remove the malformed inherited environment entry"
                        .to_owned(),
                );
            }
            block.extend_from_slice(&key_units);
            block.push(u16::from(b'='));
            block.extend_from_slice(&value_units);
            block.push(0);
        }
        block.push(0);
        Ok(block)
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

    fn local_app_data_path() -> Result<PathBuf, String> {
        let mut raw_path = null_mut();
        // SAFETY: the folder GUID and output pointer are exact, and no token is
        // supplied. This function is called only after Job containment is proved.
        let hresult = unsafe {
            SHGetKnownFolderPath(
                &raw const FOLDER_ID_LOCAL_APP_DATA,
                KF_FLAG_DONT_VERIFY,
                null_mut(),
                &raw mut raw_path,
            )
        };
        if hresult < 0 || raw_path.is_null() {
            if !raw_path.is_null() {
                // SAFETY: Shell32 returned this allocation through CoTaskMemAlloc.
                unsafe { CoTaskMemFree(raw_path.cast()) };
            }
            return Err(format!(
                "SHGetKnownFolderPath_failed hresult=0x{:08X}",
                hresult.cast_unsigned()
            ));
        }
        let mut length = 0usize;
        // SAFETY: SHGetKnownFolderPath returned a null-terminated UTF-16 string.
        while unsafe { *raw_path.add(length) } != 0 {
            length += 1;
            if length > MAX_KNOWN_FOLDER_PATH_UNITS {
                // SAFETY: Shell32 returned this allocation through CoTaskMemAlloc.
                unsafe { CoTaskMemFree(raw_path.cast()) };
                return Err(format!(
                    "SHGetKnownFolderPath_length_exceeded max_units={MAX_KNOWN_FOLDER_PATH_UNITS}"
                ));
            }
        }
        // SAFETY: the bounded scan above proved this exact initialized range.
        let path = PathBuf::from(OsString::from_wide(unsafe {
            std::slice::from_raw_parts(raw_path, length)
        }));
        // SAFETY: Shell32 returned this allocation through CoTaskMemAlloc.
        unsafe { CoTaskMemFree(raw_path.cast()) };
        if !path.is_absolute() {
            return Err("SHGetKnownFolderPath_returned_non_absolute_path".to_owned());
        }
        Ok(path)
    }

    // Callers must first prove `progress.contract_bound`. Argument-derived sinks
    // are preferred; the Known Folder sink guarantees a stable non-argv fallback.
    fn persist_contained_failure(
        paths: Option<&ErrorPaths>,
        primary_error: &str,
        progress: &BindingProgress,
    ) -> String {
        let mut report = format!("{primary_error}; {}", progress.evidence());
        if let Some(paths) = paths {
            if paths.log_path.is_absolute() {
                match append_log(&paths.log_path, &report) {
                    Ok(()) => {
                        return format!(
                            "{report}; argv_log_path={} argv_log_outcome=persisted stable_sink_authority=FOLDERID_LocalAppData stable_sink_outcome=not_attempted_argv_log_persisted",
                            paths.log_path.display()
                        );
                    }
                    Err(error) => report.push_str(&format!(
                        "; argv_log_path={} argv_log_outcome=append_failed:{error}",
                        paths.log_path.display()
                    )),
                }
            } else {
                report.push_str("; argv_log_outcome=skipped_non_absolute");
            }

            let fallback_path = paths
                .working_directory
                .join("synapse-supervisor-bootstrap-fatal.log");
            if paths.working_directory.is_absolute() && paths.working_directory.is_dir() {
                match append_log(&fallback_path, &report) {
                    Ok(()) => {
                        return format!(
                            "{report}; argv_workdir_log_path={} argv_workdir_log_outcome=persisted stable_sink_authority=FOLDERID_LocalAppData stable_sink_outcome=not_attempted_argv_workdir_log_persisted",
                            fallback_path.display()
                        );
                    }
                    Err(error) => report.push_str(&format!(
                        "; argv_workdir_log_path={} argv_workdir_log_outcome=append_failed:{error}",
                        fallback_path.display()
                    )),
                }
            } else {
                report.push_str("; argv_workdir_log_outcome=skipped_non_absolute_or_not_directory");
            }
        } else {
            report.push_str("; argv_error_paths=absent");
        }

        let local_app_data = match local_app_data_path() {
            Ok(path) => path,
            Err(error) => {
                return format!(
                    "{report}; stable_sink_authority=FOLDERID_LocalAppData stable_sink_outcome=known_folder_failed:{error}"
                );
            }
        };
        let sink_directory = local_app_data.join("synapse").join("bootstrap-errors");
        if let Err(error) = create_dir_all(&sink_directory) {
            return format!(
                "{report}; stable_sink_authority=FOLDERID_LocalAppData stable_sink_directory={} stable_sink_outcome=create_directory_failed:{error}",
                sink_directory.display()
            );
        }
        let sink_path = sink_directory.join("synapse-supervisor-bootstrap-fatal.log");
        let stable_record = format!(
            "{report}; stable_sink_authority=FOLDERID_LocalAppData stable_sink_path={}",
            sink_path.display()
        );
        match append_log(&sink_path, &stable_record) {
            Ok(()) => format!("{stable_record}; stable_sink_outcome=persisted"),
            Err(error) => format!("{stable_record}; stable_sink_outcome=append_failed:{error}"),
        }
    }

    fn route_failure(
        primary_error: String,
        paths: Option<&ErrorPaths>,
        progress: &mut BindingProgress,
    ) -> String {
        let mut routed_error = primary_error;
        if !progress.contract_bound {
            match bind_error_only_job(progress) {
                Ok(telemetry) => routed_error.push_str(&format!("; {telemetry}")),
                Err(error) => routed_error.push_str(&format!(
                    "; error_only_job_attempt=failed error_only_job_error={error}"
                )),
            }
        }
        if progress.contract_bound {
            persist_contained_failure(paths, &routed_error, progress)
        } else {
            format!(
                "{routed_error}; {}; durable_error_persisted=false argv_sink_outcome=skipped_unbound stable_sink_outcome=skipped_unbound",
                progress.evidence()
            )
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

    fn process_is_in_job(process: Handle, job: Handle, label: &str) -> Result<bool, String> {
        let mut in_job = FALSE;
        // SAFETY: both handles and the writable BOOL are valid. A NULL Job is
        // the documented immediate/current-Job query surface.
        if unsafe { IsProcessInJob(process, job, &raw mut in_job) } == FALSE {
            return Err(format!(
                "{} membership_role={label}",
                last_error("SYNAPSE_SUPERVISOR_BOOTSTRAP_JOB_MEMBERSHIP_READBACK_FAILED")
            ));
        }
        Ok(in_job != FALSE)
    }

    fn query_job_process_ids(job: Handle, label: &str) -> Result<Vec<u32>, String> {
        // SAFETY: zero is a valid initialization for this exact class-3 ABI.
        let mut information: JobObjectBasicProcessIdList = unsafe { zeroed() };
        let mut returned = 0u32;
        let buffer_size =
            u32::try_from(size_of::<JobObjectBasicProcessIdList>()).unwrap_or(u32::MAX);
        // SAFETY: the Job handle (or documented NULL immediate Job), exact
        // class-3 buffer, and return-length output are valid.
        if unsafe {
            QueryInformationJobObject(
                job,
                JOB_OBJECT_BASIC_PROCESS_ID_LIST,
                (&raw mut information).cast(),
                buffer_size,
                &raw mut returned,
            )
        } == FALSE
        {
            return Err(format!(
                "{} job_scope={label} max_process_ids={MAX_JOB_PROCESS_IDS}",
                last_error("SYNAPSE_SUPERVISOR_BOOTSTRAP_JOB_PROCESS_LIST_READBACK_FAILED")
            ));
        }
        let count =
            usize::try_from(information.number_of_process_ids_in_list).unwrap_or(usize::MAX);
        let assigned =
            usize::try_from(information.number_of_assigned_processes).unwrap_or(usize::MAX);
        let minimum_returned = 2usize
            .checked_mul(size_of::<u32>())
            .and_then(|header| {
                count
                    .checked_mul(size_of::<usize>())
                    .and_then(|ids| header.checked_add(ids))
            })
            .unwrap_or(usize::MAX);
        if count > MAX_JOB_PROCESS_IDS
            || assigned != count
            || usize::try_from(returned).unwrap_or(usize::MAX) != minimum_returned
            || returned > buffer_size
        {
            return Err(format!(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_JOB_PROCESS_LIST_UNSTABLE job_scope={label} assigned={assigned} in_list={count} returned={returned} minimum_returned={minimum_returned} buffer_size={buffer_size} max_process_ids={MAX_JOB_PROCESS_IDS}"
            ));
        }
        let mut process_ids = Vec::with_capacity(count);
        for raw_process_id in &information.process_id_list[..count] {
            let process_id = u32::try_from(*raw_process_id).map_err(|error| {
                format!(
                    "SYNAPSE_SUPERVISOR_BOOTSTRAP_JOB_PROCESS_ID_CONVERSION_FAILED job_scope={label} raw={raw_process_id} error={error}"
                )
            })?;
            if process_id == 0 {
                return Err(format!(
                    "SYNAPSE_SUPERVISOR_BOOTSTRAP_JOB_PROCESS_ID_INVALID job_scope={label} process_id=0"
                ));
            }
            process_ids.push(process_id);
        }
        process_ids.sort_unstable();
        if process_ids.windows(2).any(|pair| pair[0] == pair[1]) {
            return Err(format!(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_JOB_PROCESS_ID_DUPLICATE job_scope={label}"
            ));
        }
        Ok(process_ids)
    }

    fn bracket_owned_job_quiescence(
        owned_job: Handle,
        bootstrap_process_id: u32,
    ) -> Result<String, String> {
        if owned_job.is_null() || is_invalid_handle(owned_job) || bootstrap_process_id == 0 {
            return Err(format!(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_OWNED_JOB_QUIESCENCE_INPUT_INVALID job_handle={owned_job:p} bootstrap_pid={bootstrap_process_id}"
            ));
        }
        // The terminated immediate child cannot create new descendants. Two
        // exact class-3 snapshots therefore bracket the whole-tree quiescence
        // boundary without opening or changing any Job query handle.
        let before = query_job_process_ids(owned_job, "owned_created_job_quiescence_before")?;
        let after = query_job_process_ids(owned_job, "owned_created_job_quiescence_after")?;
        let expected = [bootstrap_process_id];
        if before.as_slice() != expected.as_slice() || after.as_slice() != expected.as_slice() {
            return Err(format!(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_OWNED_JOB_NOT_QUIESCENT expected_process_ids={expected:?} before_process_ids={before:?} after_process_ids={after:?} requirement=two_exact_stable_class3_snapshots_with_only_current_bootstrap remediation=tear down the retained KILL_ON_JOB_CLOSE owner without routing or I/O"
            ));
        }
        Ok(format!(
            "owned_created_job_quiescence=true class3_before={before:?} class3_after={after:?} exact_expected_process_ids={expected:?} class3_parser_max_process_ids={MAX_JOB_PROCESS_IDS} job_query_handle_reopened=false"
        ))
    }

    fn stable_immediate_named_job_process_ids(named_job: Handle) -> Result<Vec<u32>, String> {
        let mut last_failure = "no_attempt".to_owned();
        for attempt in 1..=JOB_PROCESS_LIST_STABILITY_ATTEMPTS {
            // Bracket both named reads with NULL-immediate reads so any
            // membership transition during the proof invalidates the attempt.
            let snapshots = (
                query_job_process_ids(null_mut(), "immediate_before"),
                query_job_process_ids(named_job, "named_before"),
                query_job_process_ids(named_job, "named_after"),
                query_job_process_ids(null_mut(), "immediate_after"),
            );
            match snapshots {
                (Ok(immediate_before), Ok(named_before), Ok(named_after), Ok(immediate_after))
                    if immediate_before == named_before
                        && immediate_before == named_after
                        && immediate_before == immediate_after =>
                {
                    return Ok(immediate_before);
                }
                (Ok(immediate_before), Ok(named_before), Ok(named_after), Ok(immediate_after)) => {
                    last_failure = format!(
                        "attempt={attempt} immediate_before={immediate_before:?} named_before={named_before:?} immediate_after={immediate_after:?} named_after={named_after:?}"
                    );
                }
                (immediate_before, named_before, named_after, immediate_after) => {
                    last_failure = format!(
                        "attempt={attempt} immediate_before={} named_before={} immediate_after={} named_after={}",
                        immediate_before
                            .err()
                            .unwrap_or_else(|| "ok_but_guard_failed".to_owned()),
                        named_before
                            .err()
                            .unwrap_or_else(|| "ok_but_guard_failed".to_owned()),
                        immediate_after
                            .err()
                            .unwrap_or_else(|| "ok_but_guard_failed".to_owned()),
                        named_after
                            .err()
                            .unwrap_or_else(|| "ok_but_guard_failed".to_owned())
                    );
                }
            }
        }
        Err(format!(
            "SYNAPSE_SUPERVISOR_BOOTSTRAP_IMMEDIATE_NAMED_JOB_PROCESS_LIST_MISMATCH attempts={JOB_PROCESS_LIST_STABILITY_ATTEMPTS} last={last_failure} remediation=refuse a raced or physically distinct named Job"
        ))
    }

    fn has_inherited_job(mode: BootstrapMode) -> Result<bool, String> {
        let mut in_job = FALSE;
        // SAFETY: a null Job handle asks whether the current process is in any Job.
        if unsafe { IsProcessInJob(GetCurrentProcess(), null_mut(), &raw mut in_job) } == FALSE {
            return Err(format!(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_INHERITED_JOB_MEMBERSHIP_READBACK_FAILED mode={} win32={} remediation=outer_complete_tree must start outside every Job and nested_job must inherit the exact authoritative outer Job",
                mode.label(),
                io::Error::last_os_error()
            ));
        }
        Ok(in_job != FALSE)
    }

    fn verify_outer_job_context(mode: BootstrapMode) -> Result<bool, String> {
        if !matches!(
            mode,
            BootstrapMode::OuterCompleteTree | BootstrapMode::OuterCompleteTreeLegacy
        ) {
            return Err(format!(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_OUTER_CONTEXT_MODE_INVALID mode={}",
                mode.label()
            ));
        }
        // Task Scheduler may place an exec action in its own host Job. Windows
        // 8+ supports assigning this process to our stricter child Job as a
        // nested Job. The effective CPU/memory policy is the intersection of
        // the host policy and our exact 25%-CPU/768-MiB owned-tree policy, so a
        // host Job cannot weaken our limits. AssignProcessToJobObject and the
        // complete contract readback below remain the fail-closed proof.
        has_inherited_job(mode)
    }

    fn expected_inherited_parent_from_environment() -> Result<InheritedParentDescriptor, String> {
        let job_name = std::env::var_os("SYNAPSE_EXPECTED_INHERITED_JOB_NAME").ok_or_else(|| {
            "SYNAPSE_SUPERVISOR_BOOTSTRAP_NESTED_PARENT_JOB_NAME_ABSENT env=SYNAPSE_EXPECTED_INHERITED_JOB_NAME remediation=the attested broker must pass its exact created outer Job name"
                .to_owned()
        })?;
        let job_name = job_name.into_string().map_err(|_| {
            "SYNAPSE_SUPERVISOR_BOOTSTRAP_NESTED_PARENT_JOB_NAME_NON_UNICODE env=SYNAPSE_EXPECTED_INHERITED_JOB_NAME remediation=the attested broker must pass an exact Unicode Job name"
                .to_owned()
        })?;
        let outer_launch_capability_sha256 =
            std::env::var_os("SYNAPSE_OUTER_LAUNCH_CAPABILITY_SHA256")
                .ok_or_else(|| {
                    "SYNAPSE_SUPERVISOR_BOOTSTRAP_NESTED_OUTER_CAPABILITY_ABSENT env=SYNAPSE_OUTER_LAUNCH_CAPABILITY_SHA256 remediation=the attested broker must pass its permanent task launch capability"
                        .to_owned()
                })?
                .into_string()
                .map_err(|_| {
                    "SYNAPSE_SUPERVISOR_BOOTSTRAP_NESTED_OUTER_CAPABILITY_NON_UNICODE env=SYNAPSE_OUTER_LAUNCH_CAPABILITY_SHA256"
                        .to_owned()
                })?;
        validate_sha256(
            "ambient.outer_launch_capability_sha256",
            &outer_launch_capability_sha256,
        )?;
        let broker_identity_sha256 = std::env::var_os("SYNAPSE_BROKER_IDENTITY_SHA256")
            .ok_or_else(|| {
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_NESTED_BROKER_IDENTITY_ABSENT env=SYNAPSE_BROKER_IDENTITY_SHA256 remediation=the attested broker must pass its immutable semantic identity"
                    .to_owned()
            })?
            .into_string()
            .map_err(|_| {
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_NESTED_BROKER_IDENTITY_NON_UNICODE env=SYNAPSE_BROKER_IDENTITY_SHA256"
                    .to_owned()
            })?;
        validate_sha256("ambient.broker_identity_sha256", &broker_identity_sha256)?;
        if broker_identity_sha256 == outer_launch_capability_sha256 {
            return Err(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_NESTED_AMBIENT_BROKER_OUTER_CAPABILITY_COLLISION remediation=regenerate independently domain-separated broker and task capabilities"
                    .to_owned(),
            );
        }
        let prefix = format!("Local\\SynapseOwned-{outer_launch_capability_sha256}-");
        let pid_text = job_name.strip_prefix(&prefix).ok_or_else(|| {
            format!(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_NESTED_PARENT_JOB_NAME_INVALID actual={job_name} expected_prefix={prefix} expected_template={INNER_INHERITED_JOB_NAME_TEMPLATE} remediation=the attested broker must pass its exact capability-qualified outer Job name"
            )
        })?;
        let pid = pid_text.parse::<u32>().map_err(|error| {
            format!(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_NESTED_PARENT_JOB_PID_INVALID actual={pid_text} error={error} remediation=the outer Job name must contain its bootstrap PID"
            )
        })?;
        if pid == 0 || pid.to_string() != pid_text {
            return Err(format!(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_NESTED_PARENT_JOB_PID_NON_CANONICAL actual={pid_text} remediation=use the nonzero canonical decimal outer bootstrap PID"
            ));
        }
        Ok(InheritedParentDescriptor {
            job_name,
            broker_identity_sha256,
            outer_launch_capability_sha256,
            outer_bootstrap_pid: pid,
        })
    }

    fn open_inherited_parent_candidate(
        descriptor: InheritedParentDescriptor,
    ) -> Result<ParentJobCandidate, String> {
        // Nested mode has not created its own Job yet. The attested broker
        // supplies its dynamic PID-bound name, which is opened and cross-bound
        // to the current process before the nested Job can exist.
        if !has_inherited_job(BootstrapMode::NestedJob)? {
            return Err(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_NESTED_PARENT_JOB_ABSENT mode=nested_job remediation=launch --nested-job only as a child of the authoritative outer bootstrap Job"
                    .to_owned(),
            );
        }

        let job_name = descriptor.job_name.as_str();
        let wide_job_name = wide_null(OsStr::new(job_name));
        // SAFETY: the canonical terminated Job name is live for the call.
        let raw_job = unsafe { OpenJobObjectW(JOB_OBJECT_QUERY, FALSE, wide_job_name.as_ptr()) };
        if raw_job.is_null() {
            return Err(format!(
                "{} mode=nested_job expected_parent_job_name={job_name}",
                last_error("SYNAPSE_SUPERVISOR_BOOTSTRAP_NESTED_PARENT_JOB_OPEN_FAILED")
            ));
        }
        let job = OwnedHandle(raw_job);
        let proof_result = (|| -> Result<(), String> {
            let mut in_named_job = FALSE;
            // SAFETY: both the pseudo process handle and exact opened Job are valid.
            if unsafe { IsProcessInJob(GetCurrentProcess(), job.0, &raw mut in_named_job) } == FALSE
            {
                return Err(format!(
                    "{} mode=nested_job expected_parent_job_name={job_name}",
                    last_error(
                        "SYNAPSE_SUPERVISOR_BOOTSTRAP_NESTED_PARENT_NAMED_MEMBERSHIP_READBACK_FAILED"
                    )
                ));
            }
            if in_named_job == FALSE {
                return Err(format!(
                    "SYNAPSE_SUPERVISOR_BOOTSTRAP_NESTED_PARENT_NAMED_MEMBERSHIP_MISMATCH mode=nested_job expected_parent_job_name={job_name} in_job=false remediation=refuse a same-contract but physically unrelated Job"
                ));
            }

            let mut immediate_limits = JobObjectExtendedLimitInformation::default();
            let mut immediate_returned = 0u32;
            let expected_limit_size =
                u32::try_from(size_of::<JobObjectExtendedLimitInformation>()).unwrap_or(u32::MAX);
            // SAFETY: before the nested Job is created, NULL selects the current
            // process's immediate inherited Job and the class-9 buffer is exact.
            if unsafe {
                QueryInformationJobObject(
                    null_mut(),
                    JOB_OBJECT_EXTENDED_LIMIT_INFORMATION,
                    (&raw mut immediate_limits).cast(),
                    expected_limit_size,
                    &raw mut immediate_returned,
                )
            } == FALSE
            {
                return Err(format!(
                    "SYNAPSE_SUPERVISOR_BOOTSTRAP_NESTED_IMMEDIATE_PARENT_LIMIT_READBACK_FAILED mode=nested_job job_handle=NULL win32={} remediation=the immediate inherited Job class-9 contract must be readable before any nested Job is created",
                    io::Error::last_os_error()
                ));
            }
            if immediate_returned != expected_limit_size {
                return Err(format!(
                    "SYNAPSE_SUPERVISOR_BOOTSTRAP_NESTED_IMMEDIATE_PARENT_LIMIT_READBACK_SIZE_MISMATCH expected={expected_limit_size} actual={immediate_returned} mode=nested_job job_handle=NULL remediation=the immediate inherited Job must return the exact class-9 structure"
                ));
            }
            let mut immediate_cpu = JobObjectCpuRateControlInformation::default();
            immediate_returned = 0;
            let expected_cpu_size =
                u32::try_from(size_of::<JobObjectCpuRateControlInformation>()).unwrap_or(u32::MAX);
            // SAFETY: NULL still selects the same immediate inherited Job and the
            // class-15 buffer is exact.
            if unsafe {
                QueryInformationJobObject(
                    null_mut(),
                    JOB_OBJECT_CPU_RATE_CONTROL_INFORMATION,
                    (&raw mut immediate_cpu).cast(),
                    expected_cpu_size,
                    &raw mut immediate_returned,
                )
            } == FALSE
            {
                return Err(format!(
                    "SYNAPSE_SUPERVISOR_BOOTSTRAP_NESTED_IMMEDIATE_PARENT_CPU_RATE_READBACK_FAILED mode=nested_job job_handle=NULL win32={} remediation=the immediate inherited Job class-15 contract must be readable before any nested Job is created",
                    io::Error::last_os_error()
                ));
            }
            if immediate_returned != expected_cpu_size {
                return Err(format!(
                    "SYNAPSE_SUPERVISOR_BOOTSTRAP_NESTED_IMMEDIATE_PARENT_CPU_RATE_READBACK_SIZE_MISMATCH expected={expected_cpu_size} actual={immediate_returned} mode=nested_job job_handle=NULL remediation=the immediate inherited Job must return the exact class-15 structure"
                ));
            }

            let mut limits = JobObjectExtendedLimitInformation::default();
            let mut returned = 0u32;
            // SAFETY: the exact named parent Job and class-9 buffer are valid.
            if unsafe {
                QueryInformationJobObject(
                    job.0,
                    JOB_OBJECT_EXTENDED_LIMIT_INFORMATION,
                    (&raw mut limits).cast(),
                    expected_limit_size,
                    &raw mut returned,
                )
            } == FALSE
            {
                return Err(format!(
                    "SYNAPSE_SUPERVISOR_BOOTSTRAP_NESTED_PARENT_LIMIT_READBACK_FAILED mode=nested_job win32={} remediation=the inherited parent Job class-9 contract must be readable before any nested Job is created",
                    io::Error::last_os_error()
                ));
            }
            if returned != expected_limit_size {
                return Err(format!(
                    "SYNAPSE_SUPERVISOR_BOOTSTRAP_NESTED_PARENT_LIMIT_READBACK_SIZE_MISMATCH expected={expected_limit_size} actual={returned} mode=nested_job remediation=the inherited parent must return the exact class-9 structure"
                ));
            }

            let mut cpu = JobObjectCpuRateControlInformation::default();
            returned = 0;
            // SAFETY: the exact same named Job and class-15 buffer are valid.
            if unsafe {
                QueryInformationJobObject(
                    job.0,
                    JOB_OBJECT_CPU_RATE_CONTROL_INFORMATION,
                    (&raw mut cpu).cast(),
                    expected_cpu_size,
                    &raw mut returned,
                )
            } == FALSE
            {
                return Err(format!(
                    "SYNAPSE_SUPERVISOR_BOOTSTRAP_NESTED_PARENT_CPU_RATE_READBACK_FAILED mode=nested_job win32={} remediation=the inherited parent Job class-15 contract must be readable before any nested Job is created",
                    io::Error::last_os_error()
                ));
            }
            if returned != expected_cpu_size {
                return Err(format!(
                    "SYNAPSE_SUPERVISOR_BOOTSTRAP_NESTED_PARENT_CPU_RATE_READBACK_SIZE_MISMATCH expected={expected_cpu_size} actual={returned} mode=nested_job remediation=the inherited parent must return the exact class-15 structure"
                ));
            }

            let expected_memory =
            usize::try_from(SYNAPSE_OWNED_TREE_HARD_LIMIT_BYTES).map_err(|error| {
                format!(
                    "SYNAPSE_SUPERVISOR_BOOTSTRAP_NESTED_PARENT_LIMIT_CONVERSION_FAILED mode=nested_job error={error}"
                )
            })?;
            if immediate_limits.basic_limit_information.limit_flags != EXPECTED_JOB_LIMIT_FLAGS
                || immediate_limits.process_memory_limit != expected_memory
                || immediate_limits.job_memory_limit != expected_memory
                || immediate_cpu.control_flags != EXPECTED_CPU_RATE_FLAGS
                || immediate_cpu.cpu_rate != SYNAPSE_OWNED_TREE_CPU_RATE
            {
                return Err(format!(
                    "SYNAPSE_SUPERVISOR_BOOTSTRAP_NESTED_IMMEDIATE_PARENT_CONTRACT_DRIFT mode=nested_job job_handle=NULL flags=0x{:08X} process_memory={} job_memory={} cpu_flags=0x{:08X} cpu_rate={} expected_flags=0x{EXPECTED_JOB_LIMIT_FLAGS:08X} expected_process_memory={SYNAPSE_OWNED_TREE_HARD_LIMIT_BYTES} expected_job_memory={SYNAPSE_OWNED_TREE_HARD_LIMIT_BYTES} expected_cpu_flags=0x{EXPECTED_CPU_RATE_FLAGS:08X} expected_cpu_rate={SYNAPSE_OWNED_TREE_CPU_RATE} remediation=refuse to compound an absent or drifted immediate whole-owned-tree Job contract",
                    immediate_limits.basic_limit_information.limit_flags,
                    immediate_limits.process_memory_limit,
                    immediate_limits.job_memory_limit,
                    immediate_cpu.control_flags,
                    immediate_cpu.cpu_rate
                ));
            }
            if limits.basic_limit_information.limit_flags != EXPECTED_JOB_LIMIT_FLAGS
                || limits.process_memory_limit != expected_memory
                || limits.job_memory_limit != expected_memory
                || cpu.control_flags != EXPECTED_CPU_RATE_FLAGS
                || cpu.cpu_rate != SYNAPSE_OWNED_TREE_CPU_RATE
            {
                return Err(format!(
                    "SYNAPSE_SUPERVISOR_BOOTSTRAP_NESTED_NAMED_PARENT_CONTRACT_DRIFT mode=nested_job expected_parent_job_name={job_name} flags=0x{:08X} process_memory={} job_memory={} cpu_flags=0x{:08X} cpu_rate={} expected_flags=0x{EXPECTED_JOB_LIMIT_FLAGS:08X} expected_process_memory={SYNAPSE_OWNED_TREE_HARD_LIMIT_BYTES} expected_job_memory={SYNAPSE_OWNED_TREE_HARD_LIMIT_BYTES} expected_cpu_flags=0x{EXPECTED_CPU_RATE_FLAGS:08X} expected_cpu_rate={SYNAPSE_OWNED_TREE_CPU_RATE} remediation=refuse a named outer Job whose contract differs from the required immediate parent contract",
                    limits.basic_limit_information.limit_flags,
                    limits.process_memory_limit,
                    limits.job_memory_limit,
                    cpu.control_flags,
                    cpu.cpu_rate
                ));
            }
            Ok(())
        })();
        if let Err(proof_error) = proof_result {
            let close_result = job.close_checked(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_PARENT_JOB_INTERNAL_ERROR_HANDLE_CLOSE_FAILED",
            );
            return match close_result {
                Ok(()) => Err(format!(
                    "{proof_error}; named_parent_job_handle_close=verified_before_error_routing"
                )),
                Err(close_error) => Err(format!(
                    "{proof_error}; named_parent_job_handle_close_error={close_error}"
                )),
            };
        }
        Ok(ParentJobCandidate { descriptor, job })
    }

    fn attest_inherited_parent_process_lineage(
        descriptor: &InheritedParentDescriptor,
        validated: &ValidatedLaunchContract,
        current_process_id: u32,
    ) -> Result<InheritedProcessLineage, String> {
        if validated.contract.outer_launch_capability_sha256
            != descriptor.outer_launch_capability_sha256
        {
            return Err(format!(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_NESTED_OUTER_CAPABILITY_CONTRACT_MISMATCH ambient={} contract={} job_name={} remediation=the broker environment, capability-qualified Job, and immutable V2 header must agree",
                descriptor.outer_launch_capability_sha256,
                validated.contract.outer_launch_capability_sha256,
                descriptor.job_name
            ));
        }
        if validated.contract.broker_identity_sha256 != descriptor.broker_identity_sha256 {
            return Err(format!(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_NESTED_BROKER_IDENTITY_CONTRACT_MISMATCH ambient={} contract={} job_name={} remediation=the inherited broker environment and immutable inner V2 header must bind the same semantic broker identity",
                descriptor.broker_identity_sha256,
                validated.contract.broker_identity_sha256,
                descriptor.job_name
            ));
        }
        let (broker_process_id, outer_parent_process_id) =
            process_parent_chain(current_process_id)?;
        if current_process_id == broker_process_id
            || current_process_id == descriptor.outer_bootstrap_pid
            || broker_process_id == descriptor.outer_bootstrap_pid
            || outer_parent_process_id != descriptor.outer_bootstrap_pid
        {
            return Err(format!(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_NESTED_PROCESS_LINEAGE_MISMATCH current_pid={current_process_id} broker_parent_pid={broker_process_id} broker_parent_parent_pid={outer_parent_process_id} expected_outer_bootstrap_pid={} remediation=require exact current->broker_PowerShell->outer_bootstrap physical lineage",
                descriptor.outer_bootstrap_pid
            ));
        }

        let broker_process = open_process_for_image_proof(broker_process_id, "broker_powershell")?;
        let outer_process =
            open_process_for_image_proof(descriptor.outer_bootstrap_pid, "outer_bootstrap")?;
        // SAFETY: the pseudo current-process handle is always valid.
        let current_process = unsafe { GetCurrentProcess() };
        let outer_image = attest_process_image(
            outer_process.0,
            descriptor.outer_bootstrap_pid,
            &validated.bootstrap,
            "outer_bootstrap",
        )?;
        let broker_image = attest_process_image(
            broker_process.0,
            broker_process_id,
            &validated.powershell,
            "broker_powershell",
        )?;
        let current_creation_time = process_creation_time(current_process, "nested_bootstrap")?;
        if outer_image.creation_time >= broker_image.creation_time
            || broker_image.creation_time >= current_creation_time
        {
            return Err(format!(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_NESTED_PROCESS_CREATION_ORDER_INVALID outer_creation={} broker_creation={} nested_creation={} requirement=outer<broker<nested remediation=refuse PID reuse or noncausal lineage",
                outer_image.creation_time, broker_image.creation_time, current_creation_time
            ));
        }

        Ok(InheritedProcessLineage {
            broker_process_id,
            outer_process,
            broker_process,
            outer_image,
            broker_image,
            current_creation_time,
        })
    }

    fn verify_inherited_parent_job_membership(
        candidate: &ParentJobCandidate,
        lineage: &InheritedProcessLineage,
        validated: &ValidatedLaunchContract,
        current_process_id: u32,
    ) -> Result<String, String> {
        let descriptor = &candidate.descriptor;
        if validated.contract.outer_launch_capability_sha256
            != descriptor.outer_launch_capability_sha256
        {
            return Err(format!(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_NESTED_OUTER_CAPABILITY_FINAL_MISMATCH ambient={} contract={} job_name={} remediation=refuse capability drift between artifact and final Job proofs",
                descriptor.outer_launch_capability_sha256,
                validated.contract.outer_launch_capability_sha256,
                descriptor.job_name
            ));
        }
        if validated.contract.broker_identity_sha256 != descriptor.broker_identity_sha256 {
            return Err(format!(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_NESTED_BROKER_IDENTITY_FINAL_MISMATCH ambient={} contract={} job_name={} remediation=refuse inherited broker identity drift across the final Job proof",
                descriptor.broker_identity_sha256,
                validated.contract.broker_identity_sha256,
                descriptor.job_name
            ));
        }
        // SAFETY: the pseudo current-process handle is always valid.
        let current_process = unsafe { GetCurrentProcess() };
        for (process, process_id, label) in [
            (
                lineage.broker_process.0,
                lineage.broker_process_id,
                "broker_powershell_named_outer",
            ),
            (
                lineage.outer_process.0,
                descriptor.outer_bootstrap_pid,
                "outer_bootstrap_named_outer",
            ),
            (
                current_process,
                current_process_id,
                "nested_bootstrap_named_outer",
            ),
        ] {
            if !process_is_in_job(process, candidate.job.0, label)? {
                return Err(format!(
                    "SYNAPSE_SUPERVISOR_BOOTSTRAP_NESTED_LINEAGE_JOB_MEMBERSHIP_MISMATCH process_role={label} process_id={process_id} expected_parent_job_name={} in_job=false",
                    descriptor.job_name
                ));
            }
        }
        let outer_creation_time =
            process_creation_time(lineage.outer_process.0, "outer_bootstrap_final")?;
        let broker_creation_time =
            process_creation_time(lineage.broker_process.0, "broker_powershell_final")?;
        let current_creation_time =
            process_creation_time(current_process, "nested_bootstrap_final")?;
        if outer_creation_time != lineage.outer_image.creation_time
            || broker_creation_time != lineage.broker_image.creation_time
            || current_creation_time != lineage.current_creation_time
        {
            return Err(format!(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_NESTED_PROCESS_CREATION_FINAL_MISMATCH outer_before={} outer_after={outer_creation_time} broker_before={} broker_after={broker_creation_time} nested_before={} nested_after={current_creation_time} remediation=refuse process-handle or lineage drift across the final Job proof",
                lineage.outer_image.creation_time,
                lineage.broker_image.creation_time,
                lineage.current_creation_time
            ));
        }

        let process_ids = stable_immediate_named_job_process_ids(candidate.job.0)?;
        let mut expected_process_ids = vec![
            descriptor.outer_bootstrap_pid,
            lineage.broker_process_id,
            current_process_id,
        ];
        let unexpected_process_ids: Vec<u32> = process_ids
            .iter()
            .copied()
            .filter(|process_id| !expected_process_ids.contains(process_id))
            .collect();
        if unexpected_process_ids.len() != 1 {
            return Err(format!(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_NESTED_CONSOLE_HOST_COUNT_INVALID expected_count=1 actual_count={} unexpected_process_ids={unexpected_process_ids:?} immediate_named_job_process_ids={process_ids:?} remediation=require exactly one headless Windows console host for the broker PowerShell",
                unexpected_process_ids.len()
            ));
        }
        let console_host_process_id = unexpected_process_ids[0];
        let (console_host_parent_id, console_host_grandparent_id) =
            process_parent_chain(console_host_process_id)?;
        if console_host_parent_id != lineage.broker_process_id
            || console_host_grandparent_id != descriptor.outer_bootstrap_pid
        {
            return Err(format!(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_NESTED_CONSOLE_HOST_LINEAGE_MISMATCH console_host_pid={console_host_process_id} parent_pid={console_host_parent_id} grandparent_pid={console_host_grandparent_id} expected_parent_pid={} expected_grandparent_pid={} remediation=refuse an unrelated extra Job member",
                lineage.broker_process_id, descriptor.outer_bootstrap_pid
            ));
        }
        let system_root = std::env::var_os("SystemRoot").ok_or_else(|| {
            "SYNAPSE_SUPERVISOR_BOOTSTRAP_SYSTEM_ROOT_MISSING remediation=restore the Windows SystemRoot environment before launching the immutable broker task".to_owned()
        })?;
        let console_host_path = PathBuf::from(system_root)
            .join("System32")
            .join("conhost.exe");
        let console_host_lease = open_file_lease(&console_host_path, false, "broker_conhost")?;
        let console_host_process =
            open_process_for_image_proof(console_host_process_id, "broker_conhost")?;
        let console_host_image = attest_process_image(
            console_host_process.0,
            console_host_process_id,
            &console_host_lease,
            "broker_conhost",
        )?;
        if console_host_image.creation_time < lineage.broker_image.creation_time
            || console_host_image.creation_time > current_creation_time
            || !process_is_in_job(
                console_host_process.0,
                candidate.job.0,
                "broker_conhost_named_outer",
            )?
        {
            return Err(format!(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_NESTED_CONSOLE_HOST_IDENTITY_INVALID console_host_pid={console_host_process_id} creation_time={} broker_creation_time={} nested_creation_time={current_creation_time} path={} mapped_nt={} remediation=refuse a console host that is not the exact creation-ordered protected System32 image in the outer Job",
                console_host_image.creation_time,
                lineage.broker_image.creation_time,
                console_host_image.full_dos_path.display(),
                console_host_image.mapped_nt_path.to_string_lossy()
            ));
        }
        expected_process_ids.push(console_host_process_id);
        expected_process_ids.sort_unstable();
        if process_ids != expected_process_ids {
            return Err(format!(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_NESTED_JOB_PROCESS_LIST_EXACT_MISMATCH expected_process_ids={expected_process_ids:?} actual_process_ids={process_ids:?} expected_roles=outer_bootstrap,broker_powershell,broker_conhost,nested_bootstrap expected_parent_job_name={} requirement=no_missing_or_unexpected_outer_job_member remediation=serialize broker launches and refuse an ambiguous outer Job lineage",
                descriptor.job_name
            ));
        }
        Ok(format!(
            "SYNAPSE_SUPERVISOR_BOOTSTRAP_NESTED_LINEAGE_VERIFIED expected_parent_job_name={} broker_identity_sha256={} outer_launch_capability_sha256={} inner_generation_launch_capability_sha256={} capability_domains_pairwise_distinct=true ambient_broker_identity_crossbound=true outer_bootstrap_pid={} outer_creation_time={} outer_path={} outer_mapped_nt={} outer_file_id_128={} outer_sha256={} broker_powershell_pid={} broker_creation_time={} broker_path={} broker_mapped_nt={} powershell_file_id_128={} powershell_sha256={} broker_conhost_pid={} broker_conhost_creation_time={} broker_conhost_path={} broker_conhost_mapped_nt={} broker_conhost_file_id_128={} broker_conhost_sha256={} nested_bootstrap_pid={} nested_creation_time={} process_parent_chain={}->{}->{} inherited_job_count=1 inherited_job_count_basis=attested_outer_entry_rejects_inherited_job_then_creates_exact_capability_named_outer_plus_current_NULL_immediate_crossbind unexpected_job_member_count=0 unexpected_job_ancestor_count=0 unexpected_job_ancestor_basis=attested_outer_entry_contract_plus_exact_outer_image_and_lineage immediate_named_job_process_ids={process_ids:?} exact_expected_process_ids={expected_process_ids:?} bracketed_NULL_named_class3_equality=true immediate_named_list_stability_attempts_max={JOB_PROCESS_LIST_STABILITY_ATTEMPTS} named_job_proof_handle_close=required_before_inner_job kernel_job_identity_boundary=capability_unique_name_plus_named_membership_plus_exact_NULL_and_named_contracts_plus_stable_process_list_equality;nonhostile_same_user_required",
            descriptor.job_name,
            descriptor.broker_identity_sha256,
            descriptor.outer_launch_capability_sha256,
            validated.contract.launch_capability_sha256,
            lineage.outer_image.process_id,
            lineage.outer_image.creation_time,
            lineage.outer_image.full_dos_path.display(),
            lineage.outer_image.mapped_nt_path.to_string_lossy(),
            validated.bootstrap.file_id_128,
            validated.bootstrap.sha256,
            lineage.broker_image.process_id,
            lineage.broker_image.creation_time,
            lineage.broker_image.full_dos_path.display(),
            lineage.broker_image.mapped_nt_path.to_string_lossy(),
            validated.powershell.file_id_128,
            validated.powershell.sha256,
            console_host_process_id,
            console_host_image.creation_time,
            console_host_image.full_dos_path.display(),
            console_host_image.mapped_nt_path.to_string_lossy(),
            console_host_lease.file_id_128,
            console_host_lease.sha256,
            current_process_id,
            lineage.current_creation_time,
            current_process_id,
            lineage.broker_process_id,
            descriptor.outer_bootstrap_pid
        ))
    }

    fn create_job_object(
        process_id: u32,
        mode_label: &str,
        name_prefix: &str,
    ) -> Result<(String, ManuallyDrop<OwnedHandle>), String> {
        let job_name = format!("Local\\{name_prefix}-{process_id}");
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
            return Err(format!(
                "{} mode={mode_label} job_name={job_name}",
                last_error("SYNAPSE_SUPERVISOR_BOOTSTRAP_JOB_SECURITY_DESCRIPTOR_FAILED")
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
            return Err(format!(
                "{} mode={mode_label} job_name={job_name}",
                explicit_win32_error(
                    "SYNAPSE_SUPERVISOR_BOOTSTRAP_JOB_CREATE_FAILED",
                    create_job_error,
                )
            ));
        }
        if create_job_error == ERROR_ALREADY_EXISTS {
            let collision = format!(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_JOB_NAME_COLLISION mode={mode_label} job_name={job_name} win32={create_job_error} remediation=refuse ambiguous kernel ownership; inspect a stale same-PID Job before restarting"
            );
            return match OwnedHandle(raw_job)
                .close_checked("SYNAPSE_SUPERVISOR_BOOTSTRAP_JOB_COLLISION_HANDLE_CLOSE_FAILED")
            {
                Ok(()) => Err(format!(
                    "{collision}; collision_handle_close=verified_before_error_routing"
                )),
                Err(close_error) => Err(format!(
                    "{collision}; collision_handle_close_error={close_error}"
                )),
            };
        }
        // This is the sole lifetime-owning handle for a KILL_ON_JOB_CLOSE Job.
        // It is deliberately leaked until process teardown so every error path
        // retains containment while stderr and any post-bind durable record run.
        Ok((job_name, ManuallyDrop::new(OwnedHandle(raw_job))))
    }

    fn set_and_verify_parent_job(
        job: Handle,
        mode_label: &str,
        job_name: &str,
        expected_cpu_rate: u32,
        progress: &mut BindingProgress,
    ) -> Result<JobObjectMemoryUsageInformation, String> {
        let mut limits = JobObjectExtendedLimitInformation::default();
        limits.basic_limit_information.limit_flags = EXPECTED_JOB_LIMIT_FLAGS;
        let expected_memory =
            usize::try_from(SYNAPSE_OWNED_TREE_HARD_LIMIT_BYTES).map_err(|error| {
                format!(
                    "SYNAPSE_SUPERVISOR_BOOTSTRAP_LIMIT_CONVERSION_FAILED mode={mode_label} error={error}"
                )
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
            return Err(format!(
                "{} mode={mode_label}",
                last_error("SYNAPSE_SUPERVISOR_BOOTSTRAP_JOB_LIMIT_SET_FAILED")
            ));
        }
        let cpu = JobObjectCpuRateControlInformation {
            control_flags: EXPECTED_CPU_RATE_FLAGS,
            cpu_rate: expected_cpu_rate,
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
            return Err(format!(
                "{} mode={mode_label} expected_cpu_rate={expected_cpu_rate}",
                last_error("SYNAPSE_SUPERVISOR_BOOTSTRAP_CPU_RATE_SET_FAILED")
            ));
        }
        // SAFETY: pseudo current-process handle and owned Job handle are valid.
        if unsafe { AssignProcessToJobObject(job, GetCurrentProcess()) } == FALSE {
            return Err(format!(
                "{} mode={mode_label}",
                last_error("SYNAPSE_SUPERVISOR_BOOTSTRAP_ASSIGN_SELF_FAILED")
            ));
        }
        let mut in_job = FALSE;
        // SAFETY: pointers are valid for the call.
        if unsafe { IsProcessInJob(GetCurrentProcess(), job, &raw mut in_job) } == FALSE {
            return Err(format!(
                "{} mode={mode_label}",
                last_error("SYNAPSE_SUPERVISOR_BOOTSTRAP_MEMBERSHIP_READBACK_FAILED")
            ));
        }
        if in_job == FALSE {
            return Err(format!(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_MEMBERSHIP_READBACK_FAILED mode={mode_label} in_job=false remediation=refuse file I/O or child launch until exact self-membership is proved"
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
            return Err(format!(
                "{} mode={mode_label}",
                last_error("SYNAPSE_SUPERVISOR_BOOTSTRAP_JOB_LIMIT_READBACK_FAILED")
            ));
        }
        if returned != expected_limit_size {
            return Err(format!(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_JOB_LIMIT_READBACK_SIZE_MISMATCH mode={mode_label} expected={expected_limit_size} actual={returned} remediation=the host ABI must return the exact class-9 structure"
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
            return Err(format!(
                "{} mode={mode_label}",
                last_error("SYNAPSE_SUPERVISOR_BOOTSTRAP_CPU_RATE_READBACK_FAILED")
            ));
        }
        if returned != expected_cpu_size {
            return Err(format!(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_CPU_RATE_READBACK_SIZE_MISMATCH mode={mode_label} expected={expected_cpu_size} actual={returned} remediation=the host ABI must return the exact class-15 structure"
            ));
        }
        if limit_readback.basic_limit_information.limit_flags != EXPECTED_JOB_LIMIT_FLAGS
            || limit_readback.process_memory_limit != expected_memory
            || limit_readback.job_memory_limit != expected_memory
            || cpu_readback.control_flags != EXPECTED_CPU_RATE_FLAGS
            || cpu_readback.cpu_rate != expected_cpu_rate
        {
            return Err(format!(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_JOB_CONTRACT_DRIFT mode={} flags=0x{:08X} process_memory={} job_memory={} cpu_flags=0x{:08X} cpu_rate={} expected_cpu_rate={expected_cpu_rate} working_set_policy=measured_only remediation=the kernel did not retain the exact compiled parent Job contract",
                mode_label,
                limit_readback.basic_limit_information.limit_flags,
                limit_readback.process_memory_limit,
                limit_readback.job_memory_limit,
                cpu_readback.control_flags,
                cpu_readback.cpu_rate
            ));
        }
        // This is the exact monotonic durable-write boundary: self-membership
        // and both class-9/class-15 contracts are proven. Class-28 accounting
        // and process telemetry below may fail without revoking containment.
        progress.mark_contract_bound(mode_label, job_name, expected_cpu_rate);
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
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_JOB_MEMORY_READBACK_FAILED mode={} returned={returned} current={} peak={} earlier_extended_peak={} limit={} os_error={} remediation=class-28 accounting must be exact, within the compiled parent limit, and monotone relative to the earlier class-9 readback",
                mode_label,
                memory.job_memory,
                memory.peak_job_memory_used,
                limit_readback.peak_job_memory_used,
                SYNAPSE_OWNED_TREE_HARD_LIMIT_BYTES,
                io::Error::last_os_error()
            ));
        }
        Ok(memory)
    }

    fn bind_error_only_job(progress: &mut BindingProgress) -> Result<String, String> {
        // SAFETY: obtaining the current PID has no preconditions.
        let process_id = unsafe { GetCurrentProcessId() };
        let (job_name, job) = create_job_object(process_id, "error_only", "SynapseOwnedError")?;
        let memory = set_and_verify_parent_job(
            job.0,
            "error_only",
            &job_name,
            SYNAPSE_OWNED_TREE_CPU_RATE,
            progress,
        )?;
        let bootstrap_memory = bootstrap_memory_snapshot("after_error_only_bind")?;
        // `job` is ManuallyDrop: reaching either success or any post-assignment
        // error retains the exact KILL_ON_JOB_CLOSE handle through process exit.
        Ok(format!(
            "error_only_job_telemetry=complete job_security_descriptor_sddl={JOB_SECURITY_DESCRIPTOR_SDDL} current_job_memory_bytes={} peak_job_memory_bytes={} bootstrap_current_private_bytes={} bootstrap_peak_commit_bytes={}",
            memory.job_memory,
            memory.peak_job_memory_used,
            bootstrap_memory.private_usage,
            bootstrap_memory.peak_pagefile_usage
        ))
    }

    struct DebugAttachmentState {
        attached: bool,
        pending_event: Option<(u32, u32)>,
        system_process_handle: Option<(Handle, u32)>,
        system_thread_handles: Vec<(Handle, u32)>,
    }

    fn push_lifecycle_error(errors: &mut Vec<String>, stage: &str, error: String) {
        errors.push(format!("{stage}={error}"));
    }

    fn close_debugger_owned_file_handles(handles: &[(Handle, &str)]) -> Vec<String> {
        // WaitForDebugEvent transfers only CREATE_PROCESS/LOAD_DLL hFile
        // ownership to the debugger. Event hProcess/hThread remain owned by the
        // debug subsystem and are never wrapped in OwnedHandle or CloseHandle'd.
        let mut errors = Vec::new();
        let mut seen = Vec::new();
        for (handle, label) in handles.iter().copied() {
            if handle.is_null() || is_invalid_handle(handle) {
                continue;
            }
            if seen.contains(&handle) {
                continue;
            }
            seen.push(handle);
            if let Err(error) = OwnedHandle(handle).close_checked(&format!(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_DEBUG_EVENT_{}_FILE_HANDLE_CLOSE_FAILED",
                label.to_ascii_uppercase()
            )) {
                errors.push(format!("{label}={error}"));
            }
        }
        errors
    }

    fn register_system_debug_process_handle(
        state: &mut DebugAttachmentState,
        handle: Handle,
        process_id: u32,
    ) -> Result<(), String> {
        if handle.is_null() || is_invalid_handle(handle) || process_id == 0 {
            return Err(format!(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_DEBUG_SYSTEM_PROCESS_HANDLE_INVALID handle={handle:p} process_id={process_id}"
            ));
        }
        if let Some((existing_handle, existing_process_id)) = state.system_process_handle {
            if existing_handle != handle || existing_process_id != process_id {
                return Err(format!(
                    "SYNAPSE_SUPERVISOR_BOOTSTRAP_DEBUG_SYSTEM_PROCESS_HANDLE_DUPLICATE existing_handle={existing_handle:p} existing_pid={existing_process_id} actual_handle={handle:p} actual_pid={process_id}"
                ));
            }
            return Ok(());
        }
        state.system_process_handle = Some((handle, process_id));
        Ok(())
    }

    fn register_system_debug_thread_handle(
        state: &mut DebugAttachmentState,
        handle: Handle,
        thread_id: u32,
    ) -> Result<(), String> {
        if handle.is_null() || is_invalid_handle(handle) || thread_id == 0 {
            return Err(format!(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_DEBUG_SYSTEM_THREAD_HANDLE_INVALID handle={handle:p} thread_id={thread_id}"
            ));
        }
        if state
            .system_thread_handles
            .iter()
            .any(|(existing_handle, existing_thread_id)| {
                *existing_handle == handle && *existing_thread_id == thread_id
            })
        {
            return Ok(());
        }
        if state
            .system_thread_handles
            .iter()
            .any(|(existing_handle, existing_thread_id)| {
                *existing_handle == handle || *existing_thread_id == thread_id
            })
        {
            return Err(format!(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_DEBUG_SYSTEM_THREAD_HANDLE_COLLISION handle={handle:p} thread_id={thread_id} existing={:?}",
                state
                    .system_thread_handles
                    .iter()
                    .map(|(existing_handle, existing_thread_id)| {
                        format!("{existing_handle:p}:{existing_thread_id}")
                    })
                    .collect::<Vec<_>>()
            ));
        }
        if state.system_thread_handles.len() >= MAX_DEBUG_CLEANUP_EVENTS {
            return Err(format!(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_DEBUG_SYSTEM_THREAD_HANDLE_BOUND_EXCEEDED max={MAX_DEBUG_CLEANUP_EVENTS}"
            ));
        }
        state.system_thread_handles.push((handle, thread_id));
        Ok(())
    }

    fn prove_system_debug_handles_closed(
        state: &mut DebugAttachmentState,
        stage: &str,
    ) -> Result<String, String> {
        if state.attached || state.pending_event.is_some() {
            return Err(format!(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_DEBUG_SYSTEM_HANDLE_CLOSE_PROOF_PRECONDITION_FAILED stage={stage} attached={} pending_event={:?}",
                state.attached, state.pending_event
            ));
        }
        let mut errors = Vec::new();
        if let Some((handle, expected_process_id)) = state.system_process_handle {
            // SAFETY: comparing a handle with itself requires no object-specific
            // access rights. A live handle must compare equal; the debug
            // subsystem-owned value must instead be invalid after detach/exit.
            if unsafe { compare_object_handles(handle, handle) } == FALSE {
                // SAFETY: GetLastError immediately follows the failed comparison.
                let raw_error = unsafe { GetLastError() };
                if raw_error != ERROR_INVALID_HANDLE {
                    errors.push(format!(
                        "system_process_handle expected_pid={expected_process_id} handle={handle:p} expected_win32={ERROR_INVALID_HANDLE} actual_win32={raw_error}"
                    ));
                }
            } else {
                errors.push(format!(
                    "system_process_handle expected_pid={expected_process_id} handle={handle:p} still_valid_CompareObjectHandles_self=true"
                ));
            }
        }
        for (handle, expected_thread_id) in state.system_thread_handles.iter().copied() {
            // SAFETY: comparing a handle with itself requires no object-specific
            // access rights. A live handle must compare equal; the debug
            // subsystem-owned value must instead be invalid after detach/exit.
            if unsafe { compare_object_handles(handle, handle) } == FALSE {
                // SAFETY: GetLastError immediately follows the failed comparison.
                let raw_error = unsafe { GetLastError() };
                if raw_error != ERROR_INVALID_HANDLE {
                    errors.push(format!(
                        "system_thread_handle expected_tid={expected_thread_id} handle={handle:p} expected_win32={ERROR_INVALID_HANDLE} actual_win32={raw_error}"
                    ));
                }
            } else {
                errors.push(format!(
                    "system_thread_handle expected_tid={expected_thread_id} handle={handle:p} still_valid_CompareObjectHandles_self=true"
                ));
            }
        }
        if errors.is_empty() {
            let thread_count = state.system_thread_handles.len();
            let process_count = usize::from(state.system_process_handle.is_some());
            state.system_process_handle = None;
            state.system_thread_handles.clear();
            Ok(format!(
                "debug_subsystem_owned_process_handles_closed={process_count} debug_subsystem_owned_thread_handles_closed={thread_count} closure_probe=CompareObjectHandles_self_ERROR_INVALID_HANDLE debugger_manual_process_thread_CloseHandle_calls=0 double_close_proof=true"
            ))
        } else {
            Err(format!(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_DEBUG_SYSTEM_HANDLE_CLOSE_PROOF_FAILED stage={stage} errors={errors:?} remediation=refuse a detach that did not close every debug-subsystem-owned process/thread handle"
            ))
        }
    }

    fn verify_debug_create_process_event(
        event: &DebugEvent,
        create: &CreateProcessDebugInfo,
        process_information: &ProcessInformation,
        child_process: Handle,
        child_thread: Handle,
        powershell: &FileLease,
        bootstrap_process_id: u32,
    ) -> Result<String, String> {
        if event.process_id != process_information.process_id
            || event.thread_id != process_information.thread_id
        {
            return Err(format!(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_DEBUG_EVENT_ID_MISMATCH event_pid={} process_information_pid={} event_tid={} process_information_tid={} remediation=refuse a debug event that is not the exact created child",
                event.process_id,
                process_information.process_id,
                event.thread_id,
                process_information.thread_id
            ));
        }
        if create.file.is_null()
            || is_invalid_handle(create.file)
            || create.process.is_null()
            || is_invalid_handle(create.process)
            || create.thread.is_null()
            || is_invalid_handle(create.thread)
            || create.base_of_image.is_null()
        {
            return Err(format!(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_DEBUG_CREATE_HANDLES_INVALID hFile={:p} hProcess={:p} hThread={:p} lpBaseOfImage={:p} requirement=all_non_null_valid remediation=refuse an incomplete CREATE_PROCESS_DEBUG_EVENT",
                create.file, create.process, create.thread, create.base_of_image
            ));
        }
        let all_handles = [create.file, create.process, create.thread];
        if all_handles[0] == all_handles[1]
            || all_handles[0] == all_handles[2]
            || all_handles[1] == all_handles[2]
            || all_handles.contains(&child_process)
            || all_handles.contains(&child_thread)
            || all_handles.contains(&powershell.handle.0)
        {
            return Err(format!(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_DEBUG_EVENT_HANDLE_OWNERSHIP_COLLISION event_hFile={:p} event_hProcess={:p} event_hThread={:p} process_information_hProcess={child_process:p} process_information_hThread={child_thread:p} retained_powershell_hFile={:p} remediation=refuse ambiguous duplicate-handle ownership or a manual double-close boundary",
                create.file, create.process, create.thread, powershell.handle.0
            ));
        }

        // SAFETY: the retained PROCESS_INFORMATION handle grants the query
        // access required by GetProcessId.
        let pi_process_id = unsafe { GetProcessId(child_process) };
        // SAFETY: the retained PROCESS_INFORMATION handle grants the query
        // access required by GetThreadId.
        let pi_thread_id = unsafe { GetThreadId(child_thread) };
        if pi_process_id == 0
            || pi_thread_id == 0
            || pi_process_id != process_information.process_id
            || pi_thread_id != process_information.thread_id
        {
            return Err(format!(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_PROCESS_INFORMATION_HANDLE_IDENTITY_MISMATCH expected_pid={} pi_handle_pid={pi_process_id} expected_tid={} pi_handle_tid={pi_thread_id} remediation=refuse PID/TID reuse or PROCESS_INFORMATION-handle substitution",
                process_information.process_id, process_information.thread_id
            ));
        }
        // SAFETY: CompareObjectHandles imposes no object-specific access-right
        // requirement. This is essential because debug-event process/thread
        // handles are not guaranteed query-information rights.
        let same_process_object =
            unsafe { compare_object_handles(create.process, child_process) } != FALSE;
        // SAFETY: GetLastError immediately follows a failed process comparison.
        let process_compare_error = (!same_process_object).then(|| unsafe { GetLastError() });
        // SAFETY: same contract as the process-object comparison above.
        let same_thread_object =
            unsafe { compare_object_handles(create.thread, child_thread) } != FALSE;
        // SAFETY: GetLastError immediately follows a failed thread comparison.
        let thread_compare_error = (!same_thread_object).then(|| unsafe { GetLastError() });
        if !same_process_object || !same_thread_object {
            return Err(format!(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_DEBUG_HANDLE_OBJECT_IDENTITY_MISMATCH expected_pid={} expected_tid={} event_process_equals_process_information={same_process_object} process_compare_win32={process_compare_error:?} event_thread_equals_process_information={same_thread_object} thread_compare_win32={thread_compare_error:?} remediation=refuse a debug-event handle that is not the exact PROCESS_INFORMATION kernel object",
                process_information.process_id, process_information.thread_id
            ));
        }

        let pi_process_creation =
            process_creation_time(child_process, "process_information_powershell")?;
        let pi_thread_creation =
            thread_creation_time(child_thread, "process_information_primary_thread")?;
        // SAFETY: the pseudo current-process handle is always valid.
        let bootstrap_creation =
            process_creation_time(unsafe { GetCurrentProcess() }, "bootstrap_parent")?;
        if pi_process_creation <= bootstrap_creation || pi_thread_creation < pi_process_creation {
            return Err(format!(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_DEBUG_CREATION_IDENTITY_MISMATCH bootstrap_pid={bootstrap_process_id} bootstrap_creation={bootstrap_creation} child_process_creation={pi_process_creation} primary_thread_creation={pi_thread_creation} requirement=bootstrap<child_process_creation<=primary_thread_creation event_objects_crossbound_by_CompareObjectHandles=true remediation=refuse PID/TID reuse or noncausal debug lineage"
            ));
        }

        let pi_full_path =
            query_full_process_image_path(child_process, "process_information_powershell")?;
        let event_file =
            attest_file_handle(create.file, None, false, "debug_event_powershell_hFile")?;
        let mapped_nt = mapped_image_path(
            child_process,
            create.base_of_image,
            "debug_event_powershell_lpBaseOfImage",
        )?;
        if !ordinal_equals_ignore_case(
            pi_full_path.as_os_str(),
            event_file.final_dos_path.as_os_str(),
        )? || !ordinal_equals_ignore_case(
            powershell.final_dos_path.as_os_str(),
            event_file.final_dos_path.as_os_str(),
        )? || !ordinal_equals_ignore_case(&mapped_nt, &event_file.final_nt_path)?
            || !ordinal_equals_ignore_case(&mapped_nt, &powershell.final_nt_path)?
            || event_file.volume_serial_u32 != powershell.volume_serial_u32
            || event_file.file_id_128 != powershell.file_id_128
            || event_file.sha256 != powershell.sha256
            || event_file.length != powershell.length
        {
            return Err(format!(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_DEBUG_IMAGE_IDENTITY_MISMATCH pi_full_path={} hFile_final_dos={} hFile_final_nt={} mapped_nt={} retained_final_dos={} retained_final_nt={} hFile_volume={} retained_volume={} hFile_file_id_128={} retained_file_id_128={} hFile_sha256={} retained_sha256={} hFile_length={} retained_length={} remediation=refuse IFEO, image substitution, or physical-file drift",
                pi_full_path.display(),
                event_file.final_dos_path.display(),
                event_file.final_nt_path.to_string_lossy(),
                mapped_nt.to_string_lossy(),
                powershell.final_dos_path.display(),
                powershell.final_nt_path.to_string_lossy(),
                event_file.volume_serial_u32,
                powershell.volume_serial_u32,
                event_file.file_id_128,
                powershell.file_id_128,
                event_file.sha256,
                powershell.sha256,
                event_file.length,
                powershell.length
            ));
        }
        Ok(format!(
            "SYNAPSE_SUPERVISOR_BOOTSTRAP_DEBUG_SUSPENDED_CHILD_IMAGE_VERIFIED bootstrap_pid={bootstrap_process_id} supervisor_pid={} primary_thread_id={} child_creation_time={pi_process_creation} primary_thread_creation_time={pi_thread_creation} full_path={} mapped_nt={} volume_serial={} file_id_128={} sha256={} length={} first_debug_event=CREATE_PROCESS_DEBUG_EVENT debug_event_pid_tid_crossbind=true process_thread_object_crossbind=CompareObjectHandles_no_query_rights PROCESS_INFORMATION_handles=retained_separate_caller_owned_identity_cleanup lpBaseOfImage_GetMappedFileNameW_via_crossbound_PROCESS_INFORMATION_hProcess=true QueryFullProcessImageNameW_via_PROCESS_INFORMATION_hProcess=true debug_event_hFile_same_handle_attestation=true create_suspended_release_before_debug_event=true debug_event_suspension_proves_preexecution_image=true exact_retained_powershell_lease=true",
            process_information.process_id,
            process_information.thread_id,
            event_file.final_dos_path.display(),
            mapped_nt.to_string_lossy(),
            event_file.volume_serial_u32,
            event_file.file_id_128,
            event_file.sha256,
            event_file.length
        ))
    }

    fn attest_and_detach_suspended_child(
        child_process: Handle,
        child_thread: Handle,
        process_information: &ProcessInformation,
        powershell: &FileLease,
        bootstrap_process_id: u32,
        state: &mut DebugAttachmentState,
    ) -> Result<String, String> {
        // SAFETY: zero is the documented initialization for DEBUG_EVENT and the
        // bounded wait writes exactly that structure.
        let mut event: DebugEvent = unsafe { zeroed() };
        // SAFETY: CreateProcessW attached this thread as the sole debugger for
        // the exact child and the writable DEBUG_EVENT ABI is live.
        if unsafe { WaitForDebugEvent(&raw mut event, CHILD_DEBUG_EVENT_WAIT_MS) } == FALSE {
            let mut errors = vec![last_error(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_FIRST_DEBUG_EVENT_WAIT_FAILED",
            )];
            // SAFETY: no event was delivered, so the exact child can be detached
            // directly while its explicit CREATE_SUSPENDED count remains one.
            if unsafe { DebugActiveProcessStop(process_information.process_id) } == FALSE {
                errors.push(last_error(
                    "SYNAPSE_SUPERVISOR_BOOTSTRAP_DEBUG_DETACH_AFTER_WAIT_FAILED",
                ));
            } else {
                state.attached = false;
            }
            return Err(errors.join("; debug_unwind_error="));
        }
        state.pending_event = Some((event.process_id, event.thread_id));
        let mut errors = Vec::new();
        let mut evidence = None;
        if event.code == CREATE_PROCESS_DEBUG_EVENT {
            // SAFETY: the active union arm is selected by event.code.
            let create = unsafe { event.info.create_process };
            if let Err(error) =
                register_system_debug_process_handle(state, create.process, event.process_id)
            {
                push_lifecycle_error(&mut errors, "event_process_handle_register", error);
            }
            if let Err(error) =
                register_system_debug_thread_handle(state, create.thread, event.thread_id)
            {
                push_lifecycle_error(&mut errors, "event_thread_handle_register", error);
            }
            match verify_debug_create_process_event(
                &event,
                &create,
                process_information,
                child_process,
                child_thread,
                powershell,
                bootstrap_process_id,
            ) {
                Ok(value) => evidence = Some(value),
                Err(error) => push_lifecycle_error(&mut errors, "event_proof", error),
            }
            let file_close_errors =
                close_debugger_owned_file_handles(&[(create.file, "create_process_image")]);
            if file_close_errors.is_empty()
                && !create.file.is_null()
                && !is_invalid_handle(create.file)
            {
                if let Some(value) = evidence.as_mut() {
                    value.push_str("; debugger_owned_CREATE_PROCESS_hFile_checked_close_count=1");
                }
            } else {
                for close_error in file_close_errors {
                    push_lifecycle_error(&mut errors, "event_handle_close", close_error);
                }
            }
        } else {
            push_lifecycle_error(
                &mut errors,
                "first_event",
                format!(
                    "SYNAPSE_SUPERVISOR_BOOTSTRAP_FIRST_DEBUG_EVENT_INVALID expected={CREATE_PROCESS_DEBUG_EVENT} actual={} event_pid={} event_tid={} remediation=refuse any event before the exact image-creation event",
                    event.code, event.process_id, event.thread_id
                ),
            );
            let unexpected_handles = match event.code {
                CREATE_THREAD_DEBUG_EVENT => {
                    // SAFETY: the active union arm is selected by event.code.
                    let create = unsafe { event.info.create_thread };
                    if let Err(error) =
                        register_system_debug_thread_handle(state, create.thread, event.thread_id)
                    {
                        push_lifecycle_error(
                            &mut errors,
                            "unexpected_thread_handle_register",
                            error,
                        );
                    }
                    Vec::new()
                }
                LOAD_DLL_DEBUG_EVENT => {
                    // SAFETY: the active union arm is selected by event.code.
                    let load = unsafe { event.info.load_dll };
                    vec![(load.file, "unexpected_load_dll_file")]
                }
                _ => Vec::new(),
            };
            for close_error in close_debugger_owned_file_handles(&unexpected_handles) {
                push_lifecycle_error(&mut errors, "event_handle_close", close_error);
            }
        }

        // SAFETY: the exact event PID/TID pair returned by WaitForDebugEvent is
        // continued once, regardless of whether its identity proof succeeded.
        if unsafe { ContinueDebugEvent(event.process_id, event.thread_id, DBG_CONTINUE) } == FALSE {
            push_lifecycle_error(
                &mut errors,
                "continue",
                last_error("SYNAPSE_SUPERVISOR_BOOTSTRAP_FIRST_DEBUG_EVENT_CONTINUE_FAILED"),
            );
        } else {
            state.pending_event = None;
        }
        // SAFETY: detaching the exact child after its event was continued leaves
        // the independently requested CREATE_SUSPENDED count unchanged at one.
        if unsafe { DebugActiveProcessStop(process_information.process_id) } == FALSE {
            push_lifecycle_error(
                &mut errors,
                "detach",
                last_error("SYNAPSE_SUPERVISOR_BOOTSTRAP_DEBUG_DETACH_FAILED"),
            );
        } else {
            state.attached = false;
            state.pending_event = None;
            match prove_system_debug_handles_closed(state, "after_DebugActiveProcessStop") {
                Ok(close_evidence) => {
                    if let Some(value) = evidence.as_mut() {
                        value.push_str("; ");
                        value.push_str(&close_evidence);
                    }
                }
                Err(error) => push_lifecycle_error(&mut errors, "system_handle_close_proof", error),
            }
        }
        if errors.is_empty() {
            evidence.ok_or_else(|| {
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_DEBUG_EVENT_EVIDENCE_ABSENT_AFTER_SUCCESS".to_owned()
            })
        } else {
            Err(errors.join("; "))
        }
    }

    fn read_signaled_child_exit(process: Handle, stage: &str) -> Result<u32, String> {
        let mut exit_code = 0u32;
        // SAFETY: the caller proved this exact process handle is signaled.
        if unsafe { GetExitCodeProcess(process, &raw mut exit_code) } == FALSE {
            return Err(format!(
                "{} stage={stage}",
                last_error("SYNAPSE_SUPERVISOR_BOOTSTRAP_CHILD_TERMINAL_EXIT_READBACK_FAILED")
            ));
        }
        if exit_code == STILL_ACTIVE {
            return Err(format!(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_CHILD_TERMINAL_EXIT_STILL_ACTIVE stage={stage} exit_code={exit_code} remediation=refuse a signaled process without terminal exit proof"
            ));
        }
        Ok(exit_code)
    }

    fn register_system_and_close_debugger_owned_event_handles(
        event: &DebugEvent,
        state: &mut DebugAttachmentState,
    ) -> Vec<String> {
        match event.code {
            CREATE_THREAD_DEBUG_EVENT => {
                // SAFETY: the active union arm is selected by event.code.
                let create = unsafe { event.info.create_thread };
                register_system_debug_thread_handle(state, create.thread, event.thread_id)
                    .err()
                    .into_iter()
                    .collect()
            }
            CREATE_PROCESS_DEBUG_EVENT => {
                // SAFETY: the active union arm is selected by event.code.
                let create = unsafe { event.info.create_process };
                let mut errors = Vec::new();
                if let Err(error) =
                    register_system_debug_process_handle(state, create.process, event.process_id)
                {
                    errors.push(error);
                }
                if let Err(error) =
                    register_system_debug_thread_handle(state, create.thread, event.thread_id)
                {
                    errors.push(error);
                }
                errors.extend(close_debugger_owned_file_handles(&[(
                    create.file,
                    "cleanup_create_process_image",
                )]));
                errors
            }
            LOAD_DLL_DEBUG_EVENT => {
                // SAFETY: the active union arm is selected by event.code.
                let load = unsafe { event.info.load_dll };
                close_debugger_owned_file_handles(&[(load.file, "cleanup_load_dll")])
            }
            _ => Vec::new(),
        }
    }

    fn drain_terminated_debug_child(
        child_process_id: u32,
        state: &mut DebugAttachmentState,
    ) -> Result<String, String> {
        let started = Instant::now();
        let mut event_count = 0usize;
        let mut close_errors = Vec::new();
        while event_count < MAX_DEBUG_CLEANUP_EVENTS {
            let elapsed = started.elapsed().as_millis();
            if elapsed >= u128::from(CHILD_TERMINATION_WAIT_MS) {
                return Err(format!(
                    "SYNAPSE_SUPERVISOR_BOOTSTRAP_DEBUG_CLEANUP_EVENT_TIMEOUT child_pid={child_process_id} elapsed_ms={elapsed} event_count={event_count} close_errors={close_errors:?}"
                ));
            }
            let remaining = u128::from(CHILD_TERMINATION_WAIT_MS) - elapsed;
            let wait_ms = u32::try_from(remaining.max(1)).unwrap_or(u32::MAX);
            // SAFETY: zero is the documented initialization for DEBUG_EVENT.
            let mut event: DebugEvent = unsafe { zeroed() };
            // SAFETY: this thread remains the exact child's debugger and the
            // writable DEBUG_EVENT buffer is live for a bounded wait.
            if unsafe { WaitForDebugEvent(&raw mut event, wait_ms) } == FALSE {
                return Err(format!(
                    "{} child_pid={child_process_id} event_count={event_count} close_errors={close_errors:?}",
                    last_error("SYNAPSE_SUPERVISOR_BOOTSTRAP_DEBUG_CLEANUP_EVENT_WAIT_FAILED")
                ));
            }
            state.pending_event = Some((event.process_id, event.thread_id));
            close_errors.extend(register_system_and_close_debugger_owned_event_handles(
                &event, state,
            ));
            let is_exact_exit =
                event.code == EXIT_PROCESS_DEBUG_EVENT && event.process_id == child_process_id;
            // SAFETY: this exact delivered event pair is continued once.
            if unsafe { ContinueDebugEvent(event.process_id, event.thread_id, DBG_CONTINUE) }
                == FALSE
            {
                return Err(format!(
                    "{} child_pid={child_process_id} event_code={} event_pid={} event_tid={} close_errors={close_errors:?}",
                    last_error("SYNAPSE_SUPERVISOR_BOOTSTRAP_DEBUG_CLEANUP_EVENT_CONTINUE_FAILED"),
                    event.code,
                    event.process_id,
                    event.thread_id
                ));
            }
            state.pending_event = None;
            event_count += 1;
            if is_exact_exit {
                state.attached = false;
                if let Err(error) =
                    prove_system_debug_handles_closed(state, "after_EXIT_PROCESS_continue")
                {
                    close_errors.push(error);
                }
                if close_errors.is_empty() {
                    return Ok(format!(
                        "debug_exit_event_observed=true debug_cleanup_event_count={event_count}"
                    ));
                }
                return Err(format!(
                    "SYNAPSE_SUPERVISOR_BOOTSTRAP_DEBUG_CLEANUP_HANDLE_CLOSE_FAILED child_pid={child_process_id} debug_cleanup_event_count={event_count} errors={close_errors:?}"
                ));
            }
        }
        Err(format!(
            "SYNAPSE_SUPERVISOR_BOOTSTRAP_DEBUG_CLEANUP_EVENT_BOUND_EXCEEDED child_pid={child_process_id} max_events={MAX_DEBUG_CLEANUP_EVENTS} close_errors={close_errors:?}"
        ))
    }

    enum ChildCleanupFailure {
        TerminalProven(String),
        TerminalUnproved,
    }

    fn fail_fast_unproved_child_authority() -> ! {
        // No formatting, stderr, durable routing, flush/sync, or Rust unwinding
        // is permitted here. Terminating the current process makes Windows
        // close the deliberately retained sole KILL_ON_JOB_CLOSE owner handle,
        // which tears down any child authority whose process object was not
        // proved terminal. `abort` is only the no-I/O fallback if the
        // current-process termination request unexpectedly returns.
        // SAFETY: the pseudo current-process handle always denotes this process.
        unsafe {
            TerminateProcess(GetCurrentProcess(), UNPROVED_CHILD_CLEANUP_EXIT_CODE);
        }
        std::process::abort()
    }

    fn terminate_or_verify_child(
        process: Handle,
        process_id: u32,
        owned_job: Handle,
        bootstrap_process_id: u32,
        expected_exit_code: u32,
        state: &mut DebugAttachmentState,
    ) -> Result<String, ChildCleanupFailure> {
        let mut unwind_errors = Vec::new();
        let mut debug_drain_evidence = None;
        if let Some((event_process_id, event_thread_id)) = state.pending_event {
            // SAFETY: retry the exact still-pending event before any detach or
            // termination wait; success is recorded by clearing the state.
            if unsafe { ContinueDebugEvent(event_process_id, event_thread_id, DBG_CONTINUE) }
                == FALSE
            {
                push_lifecycle_error(
                    &mut unwind_errors,
                    "pending_continue_retry",
                    last_error("SYNAPSE_SUPERVISOR_BOOTSTRAP_DEBUG_PENDING_CONTINUE_RETRY_FAILED"),
                );
            } else {
                state.pending_event = None;
            }
        }
        if state.attached && state.pending_event.is_none() {
            // SAFETY: detach the exact child while its primary thread remains
            // explicitly suspended or before post-resume termination.
            if unsafe { DebugActiveProcessStop(process_id) } == FALSE {
                push_lifecycle_error(
                    &mut unwind_errors,
                    "detach_retry",
                    last_error("SYNAPSE_SUPERVISOR_BOOTSTRAP_DEBUG_DETACH_RETRY_FAILED"),
                );
            } else {
                state.attached = false;
            }
        }
        if !state.attached
            && state.pending_event.is_none()
            && (state.system_process_handle.is_some() || !state.system_thread_handles.is_empty())
        {
            match prove_system_debug_handles_closed(state, "after_cleanup_detach_retry") {
                Ok(evidence) => debug_drain_evidence = Some(evidence),
                Err(error) => {
                    push_lifecycle_error(&mut unwind_errors, "system_handle_close_proof", error);
                }
            }
        }

        // The zero-time exact-handle read is always first. A naturally exited
        // child is accepted only after a separate terminal-code readback.
        // SAFETY: the exact child process handle remains live and waitable.
        let zero_wait = unsafe { WaitForSingleObject(process, 0) };
        if zero_wait == WAIT_OBJECT_0 {
            if state.attached {
                if let Some((event_process_id, event_thread_id)) = state.pending_event {
                    // SAFETY: this is the exact outstanding event pair. Even a
                    // terminal child must not leave an event undisposed.
                    if unsafe {
                        ContinueDebugEvent(event_process_id, event_thread_id, DBG_CONTINUE)
                    } == FALSE
                    {
                        push_lifecycle_error(
                            &mut unwind_errors,
                            "already_exited_pending_continue",
                            last_error(
                                "SYNAPSE_SUPERVISOR_BOOTSTRAP_DEBUG_ALREADY_EXITED_CONTINUE_FAILED",
                            ),
                        );
                    } else {
                        state.pending_event = None;
                    }
                }
                if state.pending_event.is_none() {
                    // SAFETY: explicitly detach the exact terminal child if the
                    // debugger object still accepts it. If Windows has already
                    // advanced to exit events, drain and continue them instead.
                    if unsafe { DebugActiveProcessStop(process_id) } == FALSE {
                        push_lifecycle_error(
                            &mut unwind_errors,
                            "already_exited_detach",
                            last_error(
                                "SYNAPSE_SUPERVISOR_BOOTSTRAP_DEBUG_ALREADY_EXITED_DETACH_FAILED",
                            ),
                        );
                        match drain_terminated_debug_child(process_id, state) {
                            Ok(evidence) => debug_drain_evidence = Some(evidence),
                            Err(error) => push_lifecycle_error(
                                &mut unwind_errors,
                                "already_exited_debug_drain",
                                error,
                            ),
                        }
                    } else {
                        state.attached = false;
                    }
                }
                if !state.attached
                    && state.pending_event.is_none()
                    && (state.system_process_handle.is_some()
                        || !state.system_thread_handles.is_empty())
                {
                    match prove_system_debug_handles_closed(state, "after_already_exited_detach") {
                        Ok(evidence) => debug_drain_evidence = Some(evidence),
                        Err(error) => push_lifecycle_error(
                            &mut unwind_errors,
                            "already_exited_system_handle_close_proof",
                            error,
                        ),
                    }
                }
            }
            let job_quiescence_evidence =
                bracket_owned_job_quiescence(owned_job, bootstrap_process_id)
                    .map_err(|_| ChildCleanupFailure::TerminalUnproved)?;
            let exit_code = read_signaled_child_exit(process, "zero_time_already_exited")
                .map_err(|error| {
                    ChildCleanupFailure::TerminalProven(format!(
                        "{error}; child_pid={process_id} prior_unwind_errors={unwind_errors:?} process_object_terminal_proof=WAIT_OBJECT_0; {job_quiescence_evidence}"
                    ))
                })?;
            if unwind_errors.is_empty()
                && !state.attached
                && state.pending_event.is_none()
                && state.system_process_handle.is_none()
                && state.system_thread_handles.is_empty()
            {
                return Ok(format!(
                    "child_cleanup=already_exited child_pid={process_id} exit_code={exit_code} zero_time_wait=true process_object_terminal_proof=WAIT_OBJECT_0 debugger_detached=true debug_drain_evidence={}; {job_quiescence_evidence}",
                    debug_drain_evidence.as_deref().unwrap_or("not_required"),
                ));
            }
            return Err(ChildCleanupFailure::TerminalProven(format!(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_CHILD_ALREADY_EXITED_WITH_DEBUG_UNWIND_ERROR child_pid={process_id} exit_code={exit_code} process_object_terminal_proof=WAIT_OBJECT_0 errors={unwind_errors:?} attached={} pending_event={:?} system_process_handle_present={} system_thread_handle_count={}; {job_quiescence_evidence}",
                state.attached,
                state.pending_event,
                state.system_process_handle.is_some(),
                state.system_thread_handles.len()
            )));
        }
        if zero_wait == WAIT_FAILED {
            push_lifecycle_error(
                &mut unwind_errors,
                "zero_time_wait",
                last_error("SYNAPSE_SUPERVISOR_BOOTSTRAP_CHILD_ZERO_TIME_WAIT_FAILED"),
            );
        } else if zero_wait != WAIT_TIMEOUT {
            push_lifecycle_error(
                &mut unwind_errors,
                "zero_time_wait",
                format!(
                    "SYNAPSE_SUPERVISOR_BOOTSTRAP_CHILD_ZERO_TIME_WAIT_UNEXPECTED result={zero_wait}"
                ),
            );
        }

        // SAFETY: the caller owns this exact child process handle. The bounded
        // terminal wait below proves the physical result independently.
        let terminate_succeeded = unsafe { TerminateProcess(process, expected_exit_code) } != FALSE;
        if !terminate_succeeded {
            push_lifecycle_error(
                &mut unwind_errors,
                "terminate",
                last_error("SYNAPSE_SUPERVISOR_BOOTSTRAP_CHILD_TERMINATE_FAILED"),
            );
        }

        if state.attached {
            if let Some((event_process_id, event_thread_id)) = state.pending_event {
                // SAFETY: termination may make a previously failing continue
                // actionable; retry the exact pending pair before draining.
                if unsafe { ContinueDebugEvent(event_process_id, event_thread_id, DBG_CONTINUE) }
                    == FALSE
                {
                    push_lifecycle_error(
                        &mut unwind_errors,
                        "post_terminate_pending_continue",
                        last_error(
                            "SYNAPSE_SUPERVISOR_BOOTSTRAP_DEBUG_POST_TERMINATE_CONTINUE_FAILED",
                        ),
                    );
                } else {
                    state.pending_event = None;
                }
            }
            if state.pending_event.is_none() {
                match drain_terminated_debug_child(process_id, state) {
                    Ok(evidence) => debug_drain_evidence = Some(evidence),
                    Err(error) => push_lifecycle_error(&mut unwind_errors, "debug_drain", error),
                }
            }
        }

        // SAFETY: this exact process handle remains live for a bounded terminal wait.
        let wait = unsafe { WaitForSingleObject(process, CHILD_TERMINATION_WAIT_MS) };
        if wait == WAIT_FAILED {
            return Err(ChildCleanupFailure::TerminalUnproved);
        }
        if wait == WAIT_TIMEOUT {
            return Err(ChildCleanupFailure::TerminalUnproved);
        }
        if wait != WAIT_OBJECT_0 {
            return Err(ChildCleanupFailure::TerminalUnproved);
        }
        let job_quiescence_evidence = bracket_owned_job_quiescence(owned_job, bootstrap_process_id)
            .map_err(|_| ChildCleanupFailure::TerminalUnproved)?;
        let actual_exit_code = read_signaled_child_exit(process, "after_terminate_bounded_wait")
            .map_err(|error| {
                ChildCleanupFailure::TerminalProven(format!(
                    "{error}; child_pid={process_id} prior_unwind_errors={unwind_errors:?} process_object_terminal_proof=WAIT_OBJECT_0; {job_quiescence_evidence}"
                ))
            })?;
        if terminate_succeeded && actual_exit_code != expected_exit_code {
            push_lifecycle_error(
                &mut unwind_errors,
                "terminal_code",
                format!(
                    "SYNAPSE_SUPERVISOR_BOOTSTRAP_CHILD_TERMINATION_EXIT_MISMATCH expected={expected_exit_code} actual={actual_exit_code}"
                ),
            );
        }
        if state.attached
            || state.pending_event.is_some()
            || state.system_process_handle.is_some()
            || !state.system_thread_handles.is_empty()
        {
            push_lifecycle_error(
                &mut unwind_errors,
                "debugger_state",
                format!(
                    "SYNAPSE_SUPERVISOR_BOOTSTRAP_DEBUGGER_NOT_DETACHED_AFTER_TERMINAL_CHILD attached={} pending_event={:?} system_process_handle_present={} system_thread_handle_count={}",
                    state.attached,
                    state.pending_event,
                    state.system_process_handle.is_some(),
                    state.system_thread_handles.len()
                ),
            );
        }
        if unwind_errors.is_empty() {
            Ok(format!(
                "child_cleanup=terminated child_pid={process_id} requested_exit_code={expected_exit_code} actual_exit_code={actual_exit_code} bounded_wait_ms={CHILD_TERMINATION_WAIT_MS} process_object_terminal_proof=WAIT_OBJECT_0 debugger_detached=true debug_drain_evidence={}; {job_quiescence_evidence}",
                debug_drain_evidence.as_deref().unwrap_or("not_required"),
            ))
        } else {
            Err(ChildCleanupFailure::TerminalProven(format!(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_CHILD_CLEANUP_DIAGNOSTICS child_pid={process_id} requested_exit_code={expected_exit_code} actual_exit_code={actual_exit_code} process_object_terminal_proof=WAIT_OBJECT_0 errors={unwind_errors:?}; {job_quiescence_evidence}"
            )))
        }
    }

    fn fail_after_child_creation(
        cause: String,
        process: Handle,
        process_id: u32,
        owned_job: Handle,
        bootstrap_process_id: u32,
        expected_exit_code: u32,
        state: &mut DebugAttachmentState,
    ) -> Result<u32, String> {
        match terminate_or_verify_child(
            process,
            process_id,
            owned_job,
            bootstrap_process_id,
            expected_exit_code,
            state,
        ) {
            Ok(cleanup) => Err(format!("{cause}; {cleanup}")),
            Err(ChildCleanupFailure::TerminalProven(cleanup)) => {
                Err(format!("{cause}; child_cleanup_error={cleanup}"))
            }
            Err(ChildCleanupFailure::TerminalUnproved) => fail_fast_unproved_child_authority(),
        }
    }

    fn run(arguments: &BootstrapArgs, progress: &mut BindingProgress) -> Result<u32, String> {
        // SAFETY: obtaining the current PID has no preconditions.
        let process_id = unsafe { GetCurrentProcessId() };
        let mode = arguments.mode;
        let mode_label = mode.label();
        let expected_cpu_rate = mode.cpu_rate();
        let (expected_inherited_job_name, prevalidated, context_evidence) = match mode {
            BootstrapMode::OuterCompleteTree | BootstrapMode::OuterCompleteTreeLegacy => {
                let inherited_host_job = verify_outer_job_context(mode)?;
                let authority = if mode == BootstrapMode::OuterCompleteTree {
                    "production_broker_task"
                } else {
                    "nonproduction_probe_only"
                };
                (
                    String::new(),
                    None,
                    format!(
                        "SYNAPSE_SUPERVISOR_BOOTSTRAP_OUTER_CONTEXT_VERIFIED bootstrap_pid={process_id} mode={mode_label} authority={authority} inherited_host_job={inherited_host_job} expected_inherited_job={OUTER_INHERITED_JOB} expected_inherited_job_name=empty new_job_cpu_rate={expected_cpu_rate} cpu_rate_scope=host_effective_intersection nested_job_supported=true"
                    ),
                )
            }
            BootstrapMode::NestedJob => {
                let descriptor = expected_inherited_parent_from_environment()?;

                // Prove the named and immediate parent contracts before doing
                // artifact I/O, then close the query handle. Keeping even a
                // query-only named handle would keep KILL_ON_JOB_CLOSE alive if
                // the owning outer bootstrap crashed during attestation.
                let pre_attestation_candidate =
                    open_inherited_parent_candidate(descriptor.clone())?;
                pre_attestation_candidate.job.close_checked(
                    "SYNAPSE_SUPERVISOR_BOOTSTRAP_PREATTESTATION_PARENT_JOB_HANDLE_CLOSE_FAILED",
                )?;

                validate_paths(arguments).map_err(|error| {
                    format!(
                        "{error}; nested_parent_pre_attestation_contract_proven=true; named_job_proof_handle_closed_before_artifact_attestation=true"
                    )
                })?;
                let validated = validate_launch_contract(arguments).map_err(|error| {
                    format!(
                        "{error}; nested_parent_pre_attestation_contract_proven=true; named_job_proof_handle_closed_before_artifact_attestation=true"
                    )
                })?;

                // Attest the exact outer/bootstrap and broker/PowerShell
                // process lineage with no named Job handle open. The retained
                // process handles then cross-bind those exact PIDs and creation
                // times during the final kernel-only Job proof.
                let lineage =
                    attest_inherited_parent_process_lineage(&descriptor, &validated, process_id)?;

                // Reopen and re-prove after artifact attestation so the V2
                // header, capability-qualified name, exact lineage, and stable
                // NULL-vs-named process lists describe one current state. No
                // file-system call occurs while this named Job handle is open.
                let candidate = open_inherited_parent_candidate(descriptor)?;
                let inherited_name = candidate.descriptor.job_name.clone();
                let lineage_result = verify_inherited_parent_job_membership(
                    &candidate, &lineage, &validated, process_id,
                );
                let close_result = candidate.job.close_checked(
                    "SYNAPSE_SUPERVISOR_BOOTSTRAP_FINAL_PARENT_JOB_HANDLE_CLOSE_FAILED",
                );
                let lineage_evidence = match (lineage_result, close_result) {
                    (Ok(evidence), Ok(())) => evidence,
                    (Err(proof_error), Ok(())) => return Err(proof_error),
                    (Ok(_), Err(close_error)) => return Err(close_error),
                    (Err(proof_error), Err(close_error)) => {
                        return Err(format!(
                            "{proof_error}; parent_job_close_error={close_error}"
                        ));
                    }
                };
                drop(lineage);
                let evidence = format!(
                    "SYNAPSE_SUPERVISOR_BOOTSTRAP_NESTED_PARENT_VERIFIED bootstrap_pid={process_id} mode={mode_label} inherited_job=true expected_inherited_job={INNER_INHERITED_JOB} expected_inherited_job_name={inherited_name} immediate_job_handle=NULL immediate_contract_readback=true named_job_open=true named_job_membership=true named_contract_readback=true flags=0x{EXPECTED_JOB_LIMIT_FLAGS:08X} process_memory_limit_bytes={SYNAPSE_OWNED_TREE_HARD_LIMIT_BYTES} job_memory_limit_bytes={SYNAPSE_OWNED_TREE_HARD_LIMIT_BYTES} cpu_flags=0x{EXPECTED_CPU_RATE_FLAGS:08X} cpu_rate={SYNAPSE_OWNED_TREE_CPU_RATE} new_job_cpu_rate={expected_cpu_rate} new_job_cpu_rate_scope=parent every_named_job_handle_closed_before_artifact_io_or_inner_job=true named_job_proof_handle_closed_before_inner_job=true; {lineage_evidence}"
                );
                (inherited_name, Some(validated), evidence)
            }
        };

        let job_name_prefix = match mode {
            BootstrapMode::OuterCompleteTree => format!(
                "SynapseOwned-{}",
                arguments.launch_capability_argv.as_deref().ok_or_else(|| {
                    "SYNAPSE_SUPERVISOR_BOOTSTRAP_OUTER_CAPABILITY_ARGV_ABSENT_AFTER_PARSE"
                        .to_owned()
                })?
            ),
            BootstrapMode::OuterCompleteTreeLegacy => "SynapseOwned".to_owned(),
            BootstrapMode::NestedJob => format!(
                "SynapseOwned-{}",
                prevalidated
                    .as_ref()
                    .ok_or_else(|| {
                        "SYNAPSE_SUPERVISOR_BOOTSTRAP_NESTED_CONTRACT_ABSENT_BEFORE_JOB_CREATE"
                            .to_owned()
                    })?
                    .contract
                    .launch_capability_sha256
            ),
        };
        let (job_name, job) = create_job_object(process_id, mode_label, &job_name_prefix)
            .map_err(|error| format!("{error}; buffered_context_evidence={context_evidence}"))?;
        let memory =
            set_and_verify_parent_job(job.0, mode_label, &job_name, expected_cpu_rate, progress)
                .map_err(|error| {
                    format!("{error}; buffered_context_evidence={context_evidence}")
                })?;
        let bootstrap_memory = bootstrap_memory_snapshot("after_parent_bind")
            .map_err(|error| format!("{error}; buffered_context_evidence={context_evidence}"))?;
        let bound_evidence = format!(
            "SYNAPSE_SUPERVISOR_BOOTSTRAP_PARENT_JOB_BOUND bootstrap_pid={process_id} mode={mode_label} job_name={job_name} job_security_descriptor_sddl={JOB_SECURITY_DESCRIPTOR_SDDL} flags=0x{EXPECTED_JOB_LIMIT_FLAGS:08X} process_memory_limit_bytes={SYNAPSE_OWNED_TREE_HARD_LIMIT_BYTES} job_memory_limit_bytes={SYNAPSE_OWNED_TREE_HARD_LIMIT_BYTES} working_set_policy=measured_only cpu_flags=0x{EXPECTED_CPU_RATE_FLAGS:08X} cpu_rate={expected_cpu_rate} current_job_memory_bytes={} peak_job_memory_bytes={} bootstrap_current_private_bytes={} bootstrap_peak_commit_bytes={} bootstrap_current_working_set_bytes={} bootstrap_peak_working_set_bytes={} bootstrap_reserve_bytes={}",
            memory.job_memory,
            memory.peak_job_memory_used,
            bootstrap_memory.private_usage,
            bootstrap_memory.peak_pagefile_usage,
            bootstrap_memory.working_set_size,
            bootstrap_memory.peak_working_set_size,
            TOTAL_COMMITTED_PRIVATE_POLICY_CEILING_BYTES - SYNAPSE_OWNED_TREE_HARD_LIMIT_BYTES
        );
        let validated = if let Some(validated) = prevalidated {
            validated
        } else {
            validate_paths(arguments).map_err(|error| {
                format!(
                    "{error}; buffered_context_evidence={context_evidence}; buffered_bound_evidence={bound_evidence}"
                )
            })?;
            validate_launch_contract(arguments).map_err(|error| {
                format!(
                    "{error}; buffered_context_evidence={context_evidence}; buffered_bound_evidence={bound_evidence}"
                )
            })?
        };
        let expected_created_job_name = match mode {
            BootstrapMode::OuterCompleteTreeLegacy => {
                format!("Local\\SynapseOwned-{process_id}")
            }
            BootstrapMode::OuterCompleteTree | BootstrapMode::NestedJob => format!(
                "Local\\SynapseOwned-{}-{process_id}",
                validated.contract.launch_capability_sha256
            ),
        };
        if job_name != expected_created_job_name {
            return Err(format!(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_CREATED_JOB_NAME_MISMATCH mode={mode_label} expected={expected_created_job_name} actual={job_name} remediation=refuse a Job name that is not the exact V2 template expansion"
            ));
        }
        let contract_evidence = format!(
            "SYNAPSE_SUPERVISOR_BOOTSTRAP_LAUNCH_CONTRACT_VERIFIED schema={} role={} mode={} broker_identity_sha256={} launch_capability_sha256={} outer_launch_capability_sha256={} authority_epoch_i64={} created_job_name={} expected_inherited_job={} expected_inherited_job_name={} bootstrap_path={} bootstrap_volume_serial={} bootstrap_file_id_128={} bootstrap_sha256={} bootstrap_length={} bootstrap_mapped_nt_path={} powershell_path={} powershell_volume_serial={} powershell_file_id_128={} powershell_sha256={} powershell_length={} script_path={} script_volume_serial={} script_file_id_128={} script_sha256={} script_length={} script_body_sha256={} script_body_length={} leases=retained_through_child_exit",
            validated.contract.schema,
            validated.contract.role,
            validated.contract.mode,
            validated.contract.broker_identity_sha256,
            validated.contract.launch_capability_sha256,
            validated.contract.outer_launch_capability_sha256,
            validated.contract.authority_epoch_i64,
            job_name,
            validated.contract.expected_inherited_job,
            expected_inherited_job_name,
            validated.bootstrap.final_dos_path.display(),
            validated.bootstrap.volume_serial_u32,
            validated.bootstrap.file_id_128,
            validated.bootstrap.sha256,
            validated.bootstrap.length,
            validated.bootstrap_mapped_nt_path.to_string_lossy(),
            validated.powershell.final_dos_path.display(),
            validated.powershell.volume_serial_u32,
            validated.powershell.file_id_128,
            validated.powershell.sha256,
            validated.powershell.length,
            validated.script.final_dos_path.display(),
            validated.script.volume_serial_u32,
            validated.script.file_id_128,
            validated.script.sha256,
            validated.script.length,
            validated.contract.script_body.sha256,
            validated.contract.script_body.length
        );
        let child_environment = build_child_environment(
            mode,
            &validated,
            &job_name,
            &expected_inherited_job_name,
        )
        .map_err(|error| {
            format!(
                "{error}; buffered_context_evidence={context_evidence}; buffered_bound_evidence={bound_evidence}; buffered_contract_evidence={contract_evidence}"
            )
        })?;
        append_log(&arguments.log_path, &context_evidence).map_err(|error| {
            format!(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_LOG_WRITE_FAILED mode={mode_label} stage=context_evidence error={error}; buffered_context_evidence={context_evidence}; buffered_bound_evidence={bound_evidence}"
            )
        })?;
        append_log(&arguments.log_path, &bound_evidence).map_err(|error| {
            format!(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_LOG_WRITE_FAILED mode={mode_label} stage=bound_evidence error={error}; buffered_bound_evidence={bound_evidence}"
            )
        })?;
        append_log(&arguments.log_path, &contract_evidence).map_err(|error| {
            format!(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_LOG_WRITE_FAILED mode={mode_label} stage=contract_evidence error={error}; buffered_contract_evidence={contract_evidence}"
            )
        })?;

        let mut command = format!(
            "{} -NoProfile -ExecutionPolicy Bypass -File {} -ParentJobName {}",
            quote_windows_argument(validated.powershell.final_dos_path.as_os_str())?,
            quote_windows_argument(validated.script.final_dos_path.as_os_str())?,
            quote_windows_argument(OsStr::new(&job_name))?
        );
        let mut wide_command: Vec<u16> =
            OsStr::new(&command).encode_wide().chain(Some(0)).collect();
        command.clear();
        let wide_application = wide_null(validated.powershell.final_dos_path.as_os_str());
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
                DEBUG_ONLY_THIS_PROCESS
                    | CREATE_SUSPENDED
                    | CREATE_UNICODE_ENVIRONMENT
                    | CREATE_NO_WINDOW,
                child_environment.as_ptr().cast(),
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
        let mut debug_state = DebugAttachmentState {
            attached: true,
            pending_event: None,
            system_process_handle: None,
            system_thread_handles: Vec::new(),
        };
        let mut in_job = FALSE;
        // SAFETY: exact live process and parent Job handles are queried.
        let membership_query = unsafe { IsProcessInJob(child_process.0, job.0, &raw mut in_job) };
        if membership_query == FALSE || in_job == FALSE {
            let cause = if membership_query == FALSE {
                last_error("SYNAPSE_SUPERVISOR_BOOTSTRAP_CHILD_JOB_MEMBERSHIP_FAILED")
            } else {
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_CHILD_JOB_MEMBERSHIP_FAILED in_job=false remediation=refuse to resume a supervisor that was not creation-time associated with the authoritative parent Job".to_owned()
            };
            return fail_after_child_creation(
                cause,
                child_process.0,
                process.process_id,
                job.0,
                process_id,
                125,
                &mut debug_state,
            );
        }
        // A process created with both CREATE_SUSPENDED and a debug flag does
        // not deliver CREATE_PROCESS_DEBUG_EVENT until its explicit initial
        // suspend is released.  The debugger attachment and inherited bounded
        // Job are already authoritative cleanup boundaries here.  Releasing
        // exactly one suspend lets Windows reach the pre-execution debug event;
        // that event suspends the child again while its image is attested.
        let pre_debug_suspend_count = unsafe { ResumeThread(child_thread.0) };
        if pre_debug_suspend_count != 1 {
            let cause = if pre_debug_suspend_count == u32::MAX {
                format!(
                    "SYNAPSE_SUPERVISOR_BOOTSTRAP_PRE_DEBUG_RESUME_FAILED mode={mode_label} win32={} remediation=the exact child primary thread must have one CREATE_SUSPENDED count before debug-event attestation",
                    io::Error::last_os_error()
                )
            } else {
                format!(
                    "SYNAPSE_SUPERVISOR_BOOTSTRAP_PRE_DEBUG_RESUME_COUNT_INVALID mode={mode_label} expected_previous_suspend_count=1 actual_previous_suspend_count={pre_debug_suspend_count} remediation=terminate the ambiguous child before user-mode execution"
                )
            };
            return fail_after_child_creation(
                cause,
                child_process.0,
                process.process_id,
                job.0,
                process_id,
                126,
                &mut debug_state,
            );
        }
        let child_image_evidence = match attest_and_detach_suspended_child(
            child_process.0,
            child_thread.0,
            &process,
            &validated.powershell,
            process_id,
            &mut debug_state,
        ) {
            Ok(evidence) => evidence,
            Err(cause) => {
                return fail_after_child_creation(
                    cause,
                    child_process.0,
                    process.process_id,
                    job.0,
                    process_id,
                    124,
                    &mut debug_state,
                );
            }
        };
        if let Err(error) = append_log(&arguments.log_path, &child_image_evidence) {
            let cause = format!(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_PRE_RESUME_EVIDENCE_WRITE_FAILED mode={mode_label} error={error}; buffered_child_image_evidence={child_image_evidence}"
            );
            return fail_after_child_creation(
                cause,
                child_process.0,
                process.process_id,
                job.0,
                process_id,
                123,
                &mut debug_state,
            );
        }
        if let Err(error) = append_log(
            &arguments.log_path,
            &format!(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_CHILD_STARTED bootstrap_pid={process_id} mode={mode_label} supervisor_pid={} job_name={job_name} job_cpu_rate={expected_cpu_rate} creation_assignment=inherited_parent_job exact_membership_readback=true create_suspended_release_before_debug_event=true debug_event_preexecution_image_attested=true debugger_detach_released_child=true; {child_image_evidence}",
                process.process_id,
            ),
        ) {
            let cause = format!(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_LOG_WRITE_FAILED mode={mode_label} stage=child_started error={error}"
            );
            return fail_after_child_creation(
                cause,
                child_process.0,
                process.process_id,
                job.0,
                process_id,
                127,
                &mut debug_state,
            );
        }
        // SAFETY: the child process handle remains live for the whole wait.
        let wait = unsafe { WaitForSingleObject(child_process.0, INFINITE) };
        if wait == WAIT_FAILED {
            let cause = last_error("SYNAPSE_SUPERVISOR_BOOTSTRAP_WAIT_FAILED");
            return fail_after_child_creation(
                cause,
                child_process.0,
                process.process_id,
                job.0,
                process_id,
                128,
                &mut debug_state,
            );
        }
        if wait != WAIT_OBJECT_0 {
            let cause = format!(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_WAIT_UNEXPECTED result={wait} remediation=refuse an unproved supervisor lifetime"
            );
            return fail_after_child_creation(
                cause,
                child_process.0,
                process.process_id,
                job.0,
                process_id,
                129,
                &mut debug_state,
            );
        }
        let owned_job_quiescence_evidence = match bracket_owned_job_quiescence(job.0, process_id) {
            Ok(evidence) => evidence,
            // The immediate child is terminal, but any class-3 failure or
            // non-exact whole-tree snapshot is still TerminalUnproved.
            Err(_) => fail_fast_unproved_child_authority(),
        };
        let retained_lease_handles = [
            validated.bootstrap.handle.0,
            validated.powershell.handle.0,
            validated.script.handle.0,
        ];
        if retained_lease_handles
            .iter()
            .any(|handle| handle.is_null() || is_invalid_handle(*handle))
        {
            return Err(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_RETAINED_LEASE_HANDLE_INVALID stage=after_child_exit remediation=refuse an unproved lease lifetime"
                    .to_owned(),
            );
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
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_CHILD_EXIT bootstrap_pid={process_id} mode={mode_label} supervisor_pid={} job_cpu_rate={expected_cpu_rate} exit_code={exit_code} bootstrap_current_private_bytes={} bootstrap_peak_commit_bytes={} bootstrap_current_working_set_bytes={} bootstrap_peak_working_set_bytes={} bootstrap_reserve_bytes={} identity_lease_count={} named_parent_proof_handle_retained=false; {owned_job_quiescence_evidence}",
                process.process_id,
                final_bootstrap_memory.private_usage,
                final_bootstrap_memory.peak_pagefile_usage,
                final_bootstrap_memory.working_set_size,
                final_bootstrap_memory.peak_working_set_size,
                TOTAL_COMMITTED_PRIVATE_POLICY_CEILING_BYTES
                    - SYNAPSE_OWNED_TREE_HARD_LIMIT_BYTES,
                retained_lease_handles.len(),
            ),
        )
        .map_err(|error| {
            format!(
                "SYNAPSE_SUPERVISOR_BOOTSTRAP_LOG_WRITE_FAILED mode={mode_label} error={error}"
            )
        })?;
        Ok(exit_code)
    }

    pub fn main() {
        let mut progress = BindingProgress::default();
        let arguments = match parse_args() {
            Ok(arguments) => arguments,
            Err(failure) => {
                let ParseFailure {
                    message,
                    error_paths,
                } = failure;
                let durable_error = route_failure(message, error_paths.as_ref(), &mut progress);
                eprintln!("{durable_error}");
                std::process::exit(2);
            }
        };
        let error_paths = arguments.error_paths();
        match run(&arguments, &mut progress) {
            Ok(exit_code) => std::process::exit(i32::from_ne_bytes(exit_code.to_ne_bytes())),
            Err(error) => {
                let durable_error = route_failure(error, Some(&error_paths), &mut progress);
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
