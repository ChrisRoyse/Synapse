use std::collections::HashSet;
use std::ffi::CStr;
use std::os::raw::c_void;
use std::path::Path;
use std::ptr;

use calyx_core::Result;
use cuvs_sys as ffi;

use super::build::{
    DiskAnnBuildMetric, DiskAnnBuildParams, build_space, medoid, write_graph_from_adjacency,
    write_graph_from_adjacency_f32,
};
use super::graph::invalid;
use crate::error::{CALYX_INDEX_IO, sextant_error};

pub(super) fn build_diskann_graph_cuvs_cagra(
    path: &Path,
    vectors: &[(u32, Vec<f32>)],
    params: DiskAnnBuildParams,
    metric: DiskAnnBuildMetric,
) -> Result<()> {
    if vectors.len() == 1 {
        return write_cagra_graph(path, vectors, params, 0, &[Vec::new()], metric);
    }
    // The Vamana builder's flat space is exactly the row-major `n x dim` block
    // cuVS wants as its dataset, so this shares one allocation with it.
    let space = build_space(vectors, params.dim, metric);
    let entry = medoid(&space, metric);
    let graph_degree = params.m_max.min(vectors.len() - 1);
    let mut dataset = space.into_flat();

    let res = Resources::new()?;
    let index_params = CagraParams::new()?;
    index_params.configure(&params, graph_degree);
    let index = CagraIndex::new()?;

    let mut dataset_shape = [vectors.len() as i64, params.dim as i64];
    let mut dataset_tensor =
        host_tensor(dataset.as_mut_ptr().cast(), &mut dataset_shape, dtype_f32());
    check(
        unsafe { ffi::cuvsCagraBuild(res.0, index_params.0, &mut dataset_tensor, index.0) },
        "build",
    )?;
    check(unsafe { ffi::cuvsStreamSync(res.0) }, "sync after build")?;
    index.verify(vectors.len(), params.dim, graph_degree)?;

    let graph = index.copy_graph_to_host(&res, vectors.len(), graph_degree)?;
    let adjacency = graph_to_adjacency(&graph, vectors.len(), graph_degree, params.m_max)?;
    write_cagra_graph(path, vectors, params, entry, &adjacency, metric)
}

fn write_cagra_graph(
    path: &Path,
    vectors: &[(u32, Vec<f32>)],
    params: DiskAnnBuildParams,
    entry: u32,
    adjacency: &[Vec<u32>],
    metric: DiskAnnBuildMetric,
) -> Result<()> {
    match metric {
        DiskAnnBuildMetric::UnitL2 => {
            write_graph_from_adjacency(path, vectors, params, entry, adjacency)
        }
        DiskAnnBuildMetric::RawL2 => {
            write_graph_from_adjacency_f32(path, vectors, params, entry, adjacency)
        }
    }
}

fn graph_to_adjacency(
    graph: &[u32],
    n: usize,
    graph_degree: usize,
    m_max: usize,
) -> Result<Vec<Vec<u32>>> {
    if graph.len() != n * graph_degree {
        return Err(invalid(format!(
            "cuvs graph len {} != {n} * {graph_degree}",
            graph.len()
        )));
    }
    let mut adjacency = Vec::with_capacity(n);
    for id in 0..n {
        let mut seen = HashSet::new();
        let mut neighbors = Vec::with_capacity(m_max.min(graph_degree));
        for &candidate in &graph[id * graph_degree..(id + 1) * graph_degree] {
            let candidate_usize = candidate as usize;
            if candidate_usize >= n {
                return Err(invalid(format!(
                    "cuvs graph node {id} has out-of-range neighbor {candidate}"
                )));
            }
            if candidate_usize == id {
                continue;
            }
            if seen.insert(candidate) {
                neighbors.push(candidate);
            }
            if neighbors.len() == m_max {
                break;
            }
        }
        if n > 1 && neighbors.is_empty() {
            return Err(invalid(format!("cuvs graph node {id} has no neighbors")));
        }
        adjacency.push(neighbors);
    }
    Ok(adjacency)
}

struct Resources(ffi::cuvsResources_t);

impl Resources {
    fn new() -> Result<Self> {
        let mut res = 0;
        check(
            unsafe { ffi::cuvsResourcesCreate(&mut res) },
            "create resources",
        )?;
        Ok(Self(res))
    }
}

impl Drop for Resources {
    fn drop(&mut self) {
        let _ = unsafe { ffi::cuvsResourcesDestroy(self.0) };
    }
}

struct CagraParams(ffi::cuvsCagraIndexParams_t);

impl CagraParams {
    fn new() -> Result<Self> {
        let mut params = ptr::null_mut();
        check(
            unsafe { ffi::cuvsCagraIndexParamsCreate(&mut params) },
            "create index params",
        )?;
        if params.is_null() {
            return Err(cuvs_error("create index params", "returned null params"));
        }
        Ok(Self(params))
    }

    fn configure(&self, params: &DiskAnnBuildParams, graph_degree: usize) {
        unsafe {
            (*self.0).metric = ffi::cuvsDistanceType::L2Expanded;
            (*self.0).graph_degree = graph_degree;
            (*self.0).intermediate_graph_degree = params
                .ef_construction
                .max(graph_degree * 2)
                .max(graph_degree + 1);
            (*self.0).build_algo = ffi::cuvsCagraGraphBuildAlgo::AUTO_SELECT;
            (*self.0).nn_descent_niter = params.ef_construction.clamp(10, 64);
        }
    }
}

impl Drop for CagraParams {
    fn drop(&mut self) {
        let _ = unsafe { ffi::cuvsCagraIndexParamsDestroy(self.0) };
    }
}

struct CagraIndex(ffi::cuvsCagraIndex_t);

impl CagraIndex {
    fn new() -> Result<Self> {
        let mut index = ptr::null_mut();
        check(
            unsafe { ffi::cuvsCagraIndexCreate(&mut index) },
            "create index",
        )?;
        if index.is_null() {
            return Err(cuvs_error("create index", "returned null index"));
        }
        Ok(Self(index))
    }

    fn verify(&self, expected_n: usize, expected_dim: usize, expected_degree: usize) -> Result<()> {
        let (size, dim, degree) = self.metadata()?;
        if size != expected_n || dim != expected_dim {
            return Err(invalid(format!(
                "cuvs index metadata size={size} dim={dim}, expected {expected_n}x{expected_dim}"
            )));
        }
        if degree != expected_degree {
            return Err(invalid(format!(
                "cuvs graph_degree {degree} != requested {expected_degree}"
            )));
        }
        Ok(())
    }

    fn metadata(&self) -> Result<(usize, usize, usize)> {
        let mut size = 0_i64;
        let mut dim = 0_i64;
        let mut degree = 0_i64;
        check(
            unsafe { ffi::cuvsCagraIndexGetSize(self.0, &mut size) },
            "read index size",
        )?;
        check(
            unsafe { ffi::cuvsCagraIndexGetDims(self.0, &mut dim) },
            "read index dim",
        )?;
        check(
            unsafe { ffi::cuvsCagraIndexGetGraphDegree(self.0, &mut degree) },
            "read graph degree",
        )?;
        let size = usize::try_from(size).map_err(|_| invalid("negative cuvs index size"))?;
        let dim = usize::try_from(dim).map_err(|_| invalid("negative cuvs dim"))?;
        let degree = usize::try_from(degree).map_err(|_| invalid("negative cuvs degree"))?;
        Ok((size, dim, degree))
    }

    fn copy_graph_to_host(
        &self,
        res: &Resources,
        expected_n: usize,
        expected_degree: usize,
    ) -> Result<Vec<u32>> {
        let mut graph_view = empty_tensor();
        check(
            unsafe { ffi::cuvsCagraIndexGetGraph(self.0, &mut graph_view) },
            "read graph view",
        )?;
        let result = copy_graph_view_to_host(res, &mut graph_view, expected_n, expected_degree);
        drop_view(&mut graph_view);
        result
    }
}

impl Drop for CagraIndex {
    fn drop(&mut self) {
        let _ = unsafe { ffi::cuvsCagraIndexDestroy(self.0) };
    }
}

fn copy_graph_view_to_host(
    res: &Resources,
    graph_view: &mut ffi::DLManagedTensor,
    expected_n: usize,
    expected_degree: usize,
) -> Result<Vec<u32>> {
    ensure_graph_view(graph_view, expected_n, expected_degree)?;
    let mut host = vec![0_u32; expected_n * expected_degree];
    let mut host_shape = [expected_n as i64, expected_degree as i64];
    let mut host_tensor = host_tensor(host.as_mut_ptr().cast(), &mut host_shape, dtype_u32());
    check(
        unsafe { ffi::cuvsMatrixCopy(res.0, graph_view, &mut host_tensor) },
        "copy graph to host",
    )?;
    check(unsafe { ffi::cuvsStreamSync(res.0) }, "sync graph copy")?;
    Ok(host)
}

fn ensure_graph_view(
    graph_view: &ffi::DLManagedTensor,
    expected_n: usize,
    expected_degree: usize,
) -> Result<()> {
    let tensor = graph_view.dl_tensor;
    if tensor.ndim != 2 || tensor.shape.is_null() {
        return Err(invalid(format!(
            "cuvs graph tensor ndim={} shape_null={}",
            tensor.ndim,
            tensor.shape.is_null()
        )));
    }
    if tensor.dtype.code != ffi::DLDataTypeCode::kDLUInt as u8 || tensor.dtype.bits != 32 {
        return Err(invalid(format!(
            "cuvs graph tensor dtype code={} bits={}, expected uint32",
            tensor.dtype.code, tensor.dtype.bits
        )));
    }
    let shape = unsafe { std::slice::from_raw_parts(tensor.shape, 2) };
    if shape[0] != expected_n as i64 || shape[1] != expected_degree as i64 {
        return Err(invalid(format!(
            "cuvs graph tensor shape {:?}, expected [{expected_n}, {expected_degree}]",
            shape
        )));
    }
    Ok(())
}

fn host_tensor(
    data: *mut c_void,
    shape: &mut [i64; 2],
    dtype: ffi::DLDataType,
) -> ffi::DLManagedTensor {
    ffi::DLManagedTensor {
        dl_tensor: ffi::DLTensor {
            data,
            device: ffi::DLDevice {
                device_type: ffi::DLDeviceType::kDLCPU,
                device_id: 0,
            },
            ndim: 2,
            dtype,
            shape: shape.as_mut_ptr(),
            strides: ptr::null_mut(),
            byte_offset: 0,
        },
        manager_ctx: ptr::null_mut(),
        deleter: None,
    }
}

fn empty_tensor() -> ffi::DLManagedTensor {
    ffi::DLManagedTensor {
        dl_tensor: ffi::DLTensor {
            data: ptr::null_mut(),
            device: ffi::DLDevice {
                device_type: ffi::DLDeviceType::kDLCPU,
                device_id: 0,
            },
            ndim: 0,
            dtype: dtype_u32(),
            shape: ptr::null_mut(),
            strides: ptr::null_mut(),
            byte_offset: 0,
        },
        manager_ctx: ptr::null_mut(),
        deleter: None,
    }
}

fn dtype_f32() -> ffi::DLDataType {
    ffi::DLDataType {
        code: ffi::DLDataTypeCode::kDLFloat as u8,
        bits: 32,
        lanes: 1,
    }
}

fn dtype_u32() -> ffi::DLDataType {
    ffi::DLDataType {
        code: ffi::DLDataTypeCode::kDLUInt as u8,
        bits: 32,
        lanes: 1,
    }
}

fn drop_view(view: &mut ffi::DLManagedTensor) {
    if let Some(deleter) = view.deleter.take() {
        unsafe { deleter(view) };
    }
}

fn check(status: ffi::cuvsError_t, stage: &'static str) -> Result<()> {
    if status == ffi::cuvsError_t::CUVS_SUCCESS {
        Ok(())
    } else {
        Err(cuvs_error(stage, format!("status {status:?}")))
    }
}

fn cuvs_error(stage: &str, detail: impl std::fmt::Display) -> calyx_core::CalyxError {
    let last = unsafe {
        let ptr = ffi::cuvsGetLastErrorText();
        if ptr.is_null() {
            "no cuVS error text".to_string()
        } else {
            CStr::from_ptr(ptr).to_string_lossy().into_owned()
        }
    };
    sextant_error(
        CALYX_INDEX_IO,
        format!("diskann cuvs-cagra {stage}: {detail}; last_error={last}"),
    )
}
