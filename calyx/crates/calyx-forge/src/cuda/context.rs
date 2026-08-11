use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use cudarc::cublas::CudaBlas;
use cudarc::driver::{CudaContext as CudarcContext, CudaFunction, CudaModule};

use crate::{BackendKind, DeviceInfo, ForgeError, Result};

const BYTES_PER_MIB: u64 = 1024 * 1024;
const MIN_FREE_VRAM_MIB: u64 = 4096;
const CUDA_REMEDIATION: &str = "Check that CUDA is installed at /usr/local/cuda-13.3 and nvidia-smi shows an available CUDA GPU";

#[derive(Clone, Debug)]
pub struct CudaContext {
    inner: Arc<CudarcContext>,
    determinism: bool,
    device_idx: u32,
    name: String,
    compute_capability: (i32, i32),
    total_mem_mib: u64,
    free_mem_mib_at_init: u64,
    blas: Arc<OnceLock<Arc<CudaBlas>>>,
    distance_module: Arc<OnceLock<Arc<CudaModule>>>,
    algorithmic_module: Arc<OnceLock<Arc<CudaModule>>>,
    assay_module: Arc<OnceLock<Arc<CudaModule>>>,
    mxfp4_module: Arc<OnceLock<Arc<CudaModule>>>,
    topk_module: Arc<OnceLock<Arc<CudaModule>>>,
    quant_module: Arc<OnceLock<Arc<CudaModule>>>,
    packed_quant_module: Arc<OnceLock<Arc<CudaModule>>>,
    mxfp_quant_module: Arc<OnceLock<Arc<CudaModule>>>,
    kernel_functions: Arc<Mutex<HashMap<&'static str, Arc<CudaFunction>>>>,
}

impl CudaContext {
    pub fn inner(&self) -> &Arc<CudarcContext> {
        &self.inner
    }

    pub fn determinism(&self) -> bool {
        self.determinism
    }

    pub fn device_idx(&self) -> u32 {
        self.device_idx
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn compute_capability(&self) -> (i32, i32) {
        self.compute_capability
    }

    pub fn total_mem_mib(&self) -> u64 {
        self.total_mem_mib
    }

    pub fn free_mem_mib_at_init(&self) -> u64 {
        self.free_mem_mib_at_init
    }

    /// Live free device VRAM in bytes via `cudaMemGetInfo` (in-process — never
    /// `nvidia-smi`). The returned value reflects *current* free memory and
    /// therefore accounts for every other resident process on the GPU (the TEI
    /// containers, dcgm-exporter). This is the truth source the VRAM budgeter
    /// consults before each large dispatch; it never assumes a fixed 32 GiB.
    ///
    /// Fail-loud: a driver error surfaces as
    /// [`ForgeError::DeviceUnavailable`] (`CALYX_FORGE_DEVICE_UNAVAILABLE`) —
    /// there is no zero-fill fallback, so callers can treat the unknown state
    /// as over-budget.
    pub fn free_device_vram_bytes(&self) -> Result<usize> {
        let (free_bytes, _total_bytes) =
            self.inner
                .mem_get_info()
                .map_err(|err| ForgeError::DeviceUnavailable {
                    device: device_label(self.device_idx),
                    detail: format!("CUDA cudaMemGetInfo (live free-VRAM query) failed: {err}"),
                    remediation: CUDA_REMEDIATION.to_string(),
                })?;
        Ok(free_bytes)
    }

    pub(crate) fn blas_cache(&self) -> &OnceLock<Arc<CudaBlas>> {
        &self.blas
    }

    pub(crate) fn distance_module_cache(&self) -> &OnceLock<Arc<CudaModule>> {
        &self.distance_module
    }

    pub(crate) fn algorithmic_module_cache(&self) -> &OnceLock<Arc<CudaModule>> {
        &self.algorithmic_module
    }

    pub(crate) fn assay_module_cache(&self) -> &OnceLock<Arc<CudaModule>> {
        &self.assay_module
    }

    pub(crate) fn mxfp4_module_cache(&self) -> &OnceLock<Arc<CudaModule>> {
        &self.mxfp4_module
    }

    pub(crate) fn topk_module_cache(&self) -> &OnceLock<Arc<CudaModule>> {
        &self.topk_module
    }

    pub(crate) fn quant_module_cache(&self) -> &OnceLock<Arc<CudaModule>> {
        &self.quant_module
    }

    pub(crate) fn packed_quant_module_cache(&self) -> &OnceLock<Arc<CudaModule>> {
        &self.packed_quant_module
    }

    pub(crate) fn mxfp_quant_module_cache(&self) -> &OnceLock<Arc<CudaModule>> {
        &self.mxfp_quant_module
    }

    pub(crate) fn cached_function(
        &self,
        module: &Arc<CudaModule>,
        cache_key: &'static str,
        function_name: &'static str,
    ) -> std::result::Result<Arc<CudaFunction>, cudarc::driver::DriverError> {
        let mut functions = self
            .kernel_functions
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        if let Some(function) = functions.get(cache_key) {
            return Ok(function.clone());
        }
        let function = Arc::new(module.load_function(function_name)?);
        functions.insert(cache_key, function.clone());
        Ok(function)
    }
}

pub fn init_cuda(device_idx: u32, determinism: bool) -> Result<CudaContext> {
    let device = device_label(device_idx);
    let inner = CudarcContext::new(device_idx as usize).map_err(|err| {
        device_unavailable(device_idx, format!("CUDA context init failed: {err}"))
    })?;

    let name = inner.name().map_err(|err| {
        device_unavailable(device_idx, format!("CUDA device name query failed: {err}"))
    })?;
    let compute_capability = inner.compute_capability().map_err(|err| {
        device_unavailable(
            device_idx,
            format!("CUDA compute capability query failed: {err}"),
        )
    })?;
    let (free_bytes, total_bytes) = inner
        .mem_get_info()
        .map_err(|err| device_unavailable(device_idx, format!("CUDA VRAM query failed: {err}")))?;
    let free_mem_mib = bytes_to_mib(free_bytes);
    ensure_min_free_vram(&device, free_mem_mib)?;

    Ok(CudaContext {
        inner,
        determinism,
        device_idx,
        name,
        compute_capability,
        total_mem_mib: bytes_to_mib(total_bytes),
        free_mem_mib_at_init: free_mem_mib,
        blas: Arc::new(OnceLock::new()),
        distance_module: Arc::new(OnceLock::new()),
        algorithmic_module: Arc::new(OnceLock::new()),
        assay_module: Arc::new(OnceLock::new()),
        mxfp4_module: Arc::new(OnceLock::new()),
        topk_module: Arc::new(OnceLock::new()),
        quant_module: Arc::new(OnceLock::new()),
        packed_quant_module: Arc::new(OnceLock::new()),
        mxfp_quant_module: Arc::new(OnceLock::new()),
        kernel_functions: Arc::new(Mutex::new(HashMap::new())),
    })
}

pub fn query_device_info(ctx: &CudaContext) -> DeviceInfo {
    DeviceInfo {
        kind: BackendKind::Cuda,
        name: ctx.name.clone(),
        vram_mib: Some(ctx.total_mem_mib),
    }
}

fn ensure_min_free_vram(device: &str, free_mem_mib: u64) -> Result<()> {
    if free_mem_mib < MIN_FREE_VRAM_MIB {
        return Err(ForgeError::DeviceUnavailable {
            device: device.to_string(),
            detail: format!(
                "less than 4 GiB VRAM free; free_vram_mib={free_mem_mib}; TEI containers may be using GPU memory"
            ),
            remediation: CUDA_REMEDIATION.to_string(),
        });
    }
    Ok(())
}

fn device_unavailable(device_idx: u32, detail: String) -> ForgeError {
    ForgeError::DeviceUnavailable {
        device: device_label(device_idx),
        detail,
        remediation: CUDA_REMEDIATION.to_string(),
    }
}

fn device_label(device_idx: u32) -> String {
    format!("cuda:{device_idx}")
}

fn bytes_to_mib(bytes: usize) -> u64 {
    (bytes as u64) / BYTES_PER_MIB
}
