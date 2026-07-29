//! One cached, process-wide answer to "does this host physically have a CUDA
//! device?", so every subsystem classifies CUDA-shaped observations against the
//! same evidence.
//!
//! # Why this exists
//!
//! The math backend already resolves this question at startup: it probes NVML
//! (which ships in the NVIDIA display driver, not the CUDA toolkit) and records
//! the verdict as `SYNAPSE_CALYX_MATH_CUDA_DEVICE_ABSENT` /
//! `..._INDETERMINATE` / a present device. But that verdict was reachable only
//! through an open vault's math runtime status, so unrelated subsystems
//! re-derived CUDA expectations from environment variables alone.
//!
//! The concrete failure was #1881: `act_run_shell` logged the absence of a
//! durable `CUDA_PATH` at ERROR on a host proven at every daemon startup to
//! have no NVIDIA device, and printed a remediation telling the operator to
//! install CUDA on a machine that cannot use it. Absence of `CUDA_PATH` on a
//! device-less host is the *correct* state, and AGENTS.md is explicit that
//! classification matters as much as detection.
//!
//! The probe is cached for the process lifetime because the physical device set
//! of a running host does not change: the NVIDIA driver's device inventory is
//! fixed at driver load, and the math backend already treats a single startup
//! probe as authoritative for the whole daemon. Caching also keeps a per-shell
//! -command diagnostic from paying an NVML DLL load each time.

use std::sync::OnceLock;

use serde::Serialize;

/// NVML proved this host has no CUDA device at the probed index.
pub const HOST_CUDA_DEVICE_ABSENT: &str = "SYNAPSE_CALYX_HOST_CUDA_DEVICE_ABSENT";
/// NVML read a real CUDA device at the probed index.
pub const HOST_CUDA_DEVICE_PRESENT: &str = "SYNAPSE_CALYX_HOST_CUDA_DEVICE_PRESENT";
/// The probe could not decide. Uncertainty is never treated as absence.
pub const HOST_CUDA_DEVICE_INDETERMINATE: &str = "SYNAPSE_CALYX_HOST_CUDA_DEVICE_INDETERMINATE";

/// The host's CUDA-device verdict plus the exact basis NVML gave for it, so a
/// caller can both branch on it and cite it verbatim.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SynapseCalyxHostCudaProbe {
    /// One of the `HOST_CUDA_DEVICE_*` codes.
    pub code: &'static str,
    /// True only when NVML positively proved there is no device. Never true
    /// for an indeterminate probe.
    pub device_absent_proven: bool,
    /// True when NVML read an actual device.
    pub device_present: bool,
    /// Device name when present.
    pub device_name: Option<String>,
    /// Verbatim NVML basis for this verdict, suitable for logging as evidence.
    pub basis: String,
}

static HOST_CUDA_PROBE: OnceLock<SynapseCalyxHostCudaProbe> = OnceLock::new();

/// Returns the cached host CUDA-device verdict for device index 0, probing NVML
/// on first call.
///
/// The three verdicts are deliberately distinct and must not be collapsed:
/// only `device_absent_proven` justifies treating a missing CUDA installation
/// as the correct state.
#[must_use]
pub fn host_cuda_device_probe() -> &'static SynapseCalyxHostCudaProbe {
    HOST_CUDA_PROBE.get_or_init(|| match calyx_forge::probe_host_cuda_device(0) {
        calyx_forge::HostCudaDeviceVerdict::Present(device) => SynapseCalyxHostCudaProbe {
            code: HOST_CUDA_DEVICE_PRESENT,
            device_absent_proven: false,
            device_present: true,
            device_name: Some(device.name.clone()),
            basis: format!(
                "NVML read CUDA device 0 (name={}, uuid={}, total_mib={})",
                device.name, device.uuid, device.total_mib
            ),
        },
        calyx_forge::HostCudaDeviceVerdict::Absent { basis } => SynapseCalyxHostCudaProbe {
            code: HOST_CUDA_DEVICE_ABSENT,
            device_absent_proven: true,
            device_present: false,
            device_name: None,
            basis,
        },
        calyx_forge::HostCudaDeviceVerdict::Indeterminate { basis } => SynapseCalyxHostCudaProbe {
            code: HOST_CUDA_DEVICE_INDETERMINATE,
            device_absent_proven: false,
            device_present: false,
            device_name: None,
            basis,
        },
    })
}
