//! Emits `cfg(sextant_cuvs)` when the cuVS GPU index paths are actually
//! compiled into this build (#1130): the `cuda` feature is enabled AND the
//! target OS ships libcuvs (Linux only — RAPIDS provides no native
//! Windows/macOS packages, #1016). Independently, `cfg(sextant_cuda_pq)`
//! records that the in-tree CUDA PQ kernel was compiled. That path needs only
//! cudarc + nvcc and is supported on CUDA-capable Windows and Linux hosts.
//!
//! Source code must gate cuVS usage on `cfg(sextant_cuvs)`, never on
//! `cfg(feature = "cuda")` alone: feature flags are target-independent, so on
//! a non-Linux target the feature can be "on" while `cuvs-sys` does not exist.
//!
//! `CARGO_CFG_TARGET_OS` (not `cfg!`) is read because build scripts compile
//! for the host while this decision is about the target.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const CUDA_VERSION: &str = "13.3";
const CUDA_ARCH: &str = "sm_120";
const CUDA_CCBIN_ENV: &str = "FORGE_CUDA_CCBIN";
const MERGE_SOURCE: &str = "src/index/kernels/chunked_exact_merge.cu";
const MERGE_CUBIN_ENV: &str = "SEXTANT_CHUNKED_EXACT_MERGE_CUBIN_PATH";
const PQ_SOURCE: &str = "src/index/kernels/diskann_pq.cu";
const PQ_CUBIN_ENV: &str = "SEXTANT_DISKANN_PQ_CUBIN_PATH";

fn main() {
    println!("cargo::rustc-check-cfg=cfg(sextant_cuvs)");
    println!("cargo::rustc-check-cfg=cfg(sextant_cuda_pq)");
    println!("cargo::rerun-if-env-changed=CUDA_PATH");
    println!("cargo::rerun-if-env-changed={CUDA_CCBIN_ENV}");
    println!("cargo::rerun-if-changed={MERGE_SOURCE}");
    println!("cargo::rerun-if-changed={PQ_SOURCE}");
    let cuda_feature = std::env::var_os("CARGO_FEATURE_CUDA").is_some();
    let cuda_pq_feature = std::env::var_os("CARGO_FEATURE_CUDA_PQ").is_some();
    let target_os = std::env::var("CARGO_CFG_TARGET_OS")
        .expect("CALYX_SEXTANT_BUILD: cargo did not set CARGO_CFG_TARGET_OS");
    if cuda_feature && target_os == "linux" {
        println!("cargo::rustc-cfg=sextant_cuvs");
        compile_cuda_kernel(
            MERGE_SOURCE,
            "sextant-chunked-exact-merge.cubin",
            MERGE_CUBIN_ENV,
        );
    }
    if cuda_pq_feature || (cuda_feature && target_os == "linux") {
        println!("cargo::rustc-cfg=sextant_cuda_pq");
        compile_cuda_kernel(PQ_SOURCE, "sextant-diskann-pq.cubin", PQ_CUBIN_ENV);
    }
}

fn compile_cuda_kernel(source: &str, output: &str, output_env: &str) {
    println!("cargo:rerun-if-env-changed=CUDA_PATH");
    println!("cargo:rerun-if-changed={source}");
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("manifest dir"));
    let source = manifest.join(source);
    assert!(
        source.is_file(),
        "CUDA kernel missing: {}",
        source.display()
    );
    let out = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR")).join(output);
    let nvcc = locate_nvcc();
    let host_compiler = locate_cuda_host_compiler();
    let mut args = vec![
        format!("-arch={CUDA_ARCH}"),
        "-O3".to_string(),
        "--ftz=false".to_string(),
        "--prec-div=true".to_string(),
        "--prec-sqrt=true".to_string(),
        "--fmad=false".to_string(),
    ];
    if let Some(ccbin) = host_compiler {
        args.extend(["--compiler-bindir".to_string(), ccbin.display().to_string()]);
    }
    if !cfg!(windows) {
        args.extend(["-Xcompiler".to_string(), "-fPIC".to_string()]);
    }
    args.extend([
        "-cubin".to_string(),
        "-o".to_string(),
        out.display().to_string(),
        source.display().to_string(),
    ]);
    let output = Command::new(&nvcc)
        .args(&args)
        .output()
        .unwrap_or_else(|error| panic!("run {}: {error}", nvcc.display()));
    assert_success(&nvcc, &args, output);
    emit_kernel_path(output_env, &out);
}

fn locate_nvcc() -> PathBuf {
    let explicit = std::env::var_os("CUDA_PATH").map(PathBuf::from);
    let roots = explicit
        .clone()
        .map_or_else(default_cuda_roots, |root| vec![root]);
    let mut probed = Vec::new();
    for root in roots {
        let candidate = root
            .join("bin")
            .join(if cfg!(windows) { "nvcc.exe" } else { "nvcc" });
        if candidate.is_file() {
            return candidate;
        }
        probed.push(candidate);
    }
    panic!(
        "CALYX_SEXTANT_NVCC_NOT_FOUND: CUDA PQ needs the CUDA {CUDA_VERSION} toolkit; probed [{}]; set CUDA_PATH to its exact root",
        probed
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    );
}

fn default_cuda_roots() -> Vec<PathBuf> {
    if cfg!(windows) {
        let program_files = std::env::var_os("ProgramFiles")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("C:\\Program Files"));
        vec![
            program_files
                .join("NVIDIA GPU Computing Toolkit")
                .join("CUDA")
                .join(format!("v{CUDA_VERSION}")),
        ]
    } else {
        vec![
            PathBuf::from(format!("/usr/local/cuda-{CUDA_VERSION}")),
            PathBuf::from("/usr/local/cuda"),
        ]
    }
}

fn locate_cuda_host_compiler() -> Option<PathBuf> {
    if !cfg!(windows) {
        return None;
    }
    if let Some(path) = std::env::var_os(CUDA_CCBIN_ENV) {
        return normalize_ccbin(PathBuf::from(path)).or_else(|| {
            panic!(
                "CALYX_SEXTANT_CUDA_CCBIN_INVALID: {CUDA_CCBIN_ENV} must identify cl.exe or its directory"
            )
        });
    }
    if command_on_path("cl.exe") {
        return None;
    }
    let mut candidates = windows_msvc_ccbin_candidates();
    candidates.sort();
    candidates.pop().or_else(|| {
        panic!(
            "CALYX_SEXTANT_CUDA_HOST_COMPILER_MISSING: nvcc requires MSVC x64 cl.exe; install Visual Studio Build Tools or set {CUDA_CCBIN_ENV}"
        )
    })
}

fn normalize_ccbin(path: PathBuf) -> Option<PathBuf> {
    if path.is_dir() && path.join("cl.exe").is_file() {
        return Some(path);
    }
    if path.is_file()
        && path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.eq_ignore_ascii_case("cl.exe"))
    {
        return path.parent().map(Path::to_path_buf);
    }
    None
}

fn command_on_path(command: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path).any(|directory| directory.join(command).is_file())
    })
}

fn windows_msvc_ccbin_candidates() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(root) = std::env::var_os("ProgramFiles") {
        roots.push(PathBuf::from(root).join("Microsoft Visual Studio"));
    }
    if let Some(root) = std::env::var_os("ProgramFiles(x86)") {
        roots.push(PathBuf::from(root).join("Microsoft Visual Studio"));
    }
    let mut candidates = Vec::new();
    for root in roots {
        let Ok(years) = std::fs::read_dir(root) else {
            continue;
        };
        for year in years.flatten() {
            let Ok(editions) = std::fs::read_dir(year.path()) else {
                continue;
            };
            for edition in editions.flatten() {
                let msvc = edition.path().join("VC").join("Tools").join("MSVC");
                let Ok(versions) = std::fs::read_dir(msvc) else {
                    continue;
                };
                for version in versions.flatten() {
                    let ccbin = version.path().join("bin").join("Hostx64").join("x64");
                    if ccbin.join("cl.exe").is_file() {
                        candidates.push(ccbin);
                    }
                }
            }
        }
    }
    candidates
}

fn assert_success(nvcc: &Path, args: &[String], output: Output) {
    if output.status.success() {
        return;
    }
    panic!(
        "CALYX_SEXTANT_NVCC_FAILED: {} {}\nstatus={}\nstdout={}\nstderr={}",
        nvcc.display(),
        args.join(" "),
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn emit_kernel_path(name: &str, path: &Path) {
    println!("cargo:rustc-env={name}={}", path.display());
}
