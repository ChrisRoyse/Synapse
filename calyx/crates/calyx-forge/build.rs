use std::env;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// CUDA toolkit release this crate's kernels are compiled against.
const CUDA_VERSION: &str = "13.3";
const CUDA_ARCH: &str = "sm_120";
const CUDA_CCBIN_ENV: &str = "FORGE_CUDA_CCBIN";

struct Kernel {
    name: &'static str,
    src: &'static str,
    ptx_env: &'static str,
    cubin_env: &'static str,
}

const KERNELS: &[Kernel] = &[
    Kernel {
        name: "distance",
        src: "src/cuda/kernels/distance.cu",
        ptx_env: "FORGE_DISTANCE_PTX_PATH",
        cubin_env: "FORGE_DISTANCE_CUBIN_PATH",
    },
    Kernel {
        name: "topk",
        src: "src/cuda/kernels/topk.cu",
        ptx_env: "FORGE_TOPK_PTX_PATH",
        cubin_env: "FORGE_TOPK_CUBIN_PATH",
    },
    Kernel {
        name: "quant",
        src: "src/cuda/kernels/quant.cu",
        ptx_env: "FORGE_QUANT_PTX_PATH",
        cubin_env: "FORGE_QUANT_CUBIN_PATH",
    },
    Kernel {
        name: "packed_quant",
        src: "src/cuda/kernels/packed_quant.cu",
        ptx_env: "FORGE_PACKED_QUANT_PTX_PATH",
        cubin_env: "FORGE_PACKED_QUANT_CUBIN_PATH",
    },
    Kernel {
        name: "mxfp_quant",
        src: "src/cuda/kernels/mxfp_quant.cu",
        ptx_env: "FORGE_MXFP_QUANT_PTX_PATH",
        cubin_env: "FORGE_MXFP_QUANT_CUBIN_PATH",
    },
    Kernel {
        name: "mxfp4_gemm",
        src: "src/cuda/kernels/mxfp4_gemm.cu",
        ptx_env: "FORGE_MXFP4_GEMM_PTX_PATH",
        cubin_env: "FORGE_MXFP4_GEMM_CUBIN_PATH",
    },
    Kernel {
        name: "assay",
        src: "src/cuda/kernels/assay.cu",
        ptx_env: "FORGE_ASSAY_PTX_PATH",
        cubin_env: "FORGE_ASSAY_CUBIN_PATH",
    },
    Kernel {
        name: "algorithmic",
        src: "src/cuda/kernels/algorithmic.cu",
        ptx_env: "FORGE_ALGORITHMIC_PTX_PATH",
        cubin_env: "FORGE_ALGORITHMIC_CUBIN_PATH",
    },
];

fn main() {
    if !cuda_feature_enabled() {
        println!("cargo:warning=cuda feature not enabled, skipping kernel compilation");
        return;
    }

    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));
    let kernel_out_dir = out_dir.join("forge-cuda-kernels");
    std::fs::create_dir_all(&kernel_out_dir).expect("create CUDA kernel OUT_DIR");

    println!("cargo:rerun-if-env-changed=CUDA_PATH");
    println!("cargo:rerun-if-env-changed={CUDA_CCBIN_ENV}");
    let nvcc = locate_nvcc();
    let host_compiler = locate_cuda_host_compiler();
    warn_nvcc_version(&nvcc);

    for kernel in KERNELS {
        let src = manifest_dir.join(kernel.src);
        println!("cargo:rerun-if-changed={}", kernel.src);
        assert_source_exists(&src);

        let ptx = kernel_out_dir.join(format!("{}.ptx", kernel.name));
        let cubin = kernel_out_dir.join(format!("{}.cubin", kernel.name));

        compile_ptx(&nvcc, host_compiler.as_deref(), &src, &ptx);
        compile_cubin(&nvcc, host_compiler.as_deref(), &src, &cubin);

        println!("cargo:rustc-env={}={}", kernel.ptx_env, ptx.display());
        println!("cargo:rustc-env={}={}", kernel.cubin_env, cubin.display());
    }
}

fn cuda_feature_enabled() -> bool {
    cfg!(feature = "cuda") || env::var_os("CARGO_FEATURE_CUDA").is_some()
}

fn locate_nvcc() -> PathBuf {
    // Every candidate is built with `Path::join`, never a hand-written
    // separator, and the default roots follow the host platform's own
    // convention. Mixing the two produced `/usr/local/cuda-13.3\bin\nvcc.exe` —
    // a path that cannot exist anywhere, which sent the reader looking for a
    // broken toolchain layout instead of reading the actionable half of the
    // message (issue #1892).
    let explicit = env::var_os("CUDA_PATH").map(PathBuf::from);
    let roots: Vec<PathBuf> = match &explicit {
        Some(root) => vec![root.clone()],
        None => default_cuda_roots(),
    };

    let mut probed = Vec::new();
    for root in &roots {
        let nvcc = root.join("bin").join(nvcc_exe_name());
        if nvcc.is_file() {
            return nvcc;
        }
        probed.push(nvcc);
    }

    let probed_list = probed
        .iter()
        .map(|path| format!("  {}", path.display()))
        .collect::<Vec<_>>()
        .join("\n");
    let source = if explicit.is_some() {
        format!("CUDA_PATH is set, so only it was probed ({CUDA_VERSION} kernels)")
    } else {
        format!(
            "CUDA_PATH is unset, so the platform default {CUDA_VERSION} root(s) were probed; set \
             CUDA_PATH to a CUDA {CUDA_VERSION} root to override"
        )
    };
    panic!(
        "CALYX_FORGE_NVCC_NOT_FOUND: the `cuda` feature needs the CUDA {CUDA_VERSION} toolkit, and \
         no nvcc was found.\nprobed:\n{probed_list}\n{source}\nA host with no NVIDIA toolkit \
         cannot build this feature; build without `--features cuda` (scripts\\synapse-setup.ps1 \
         detects this and reports `Calyx CUDA kernels DISABLED` rather than failing)."
    );
}

/// Platform-conventional CUDA roots probed when `CUDA_PATH` is unset.
///
/// Windows: the NVIDIA installer's location under `%ProgramFiles%`, which is
/// also where it points `CUDA_PATH`. Unix: the `/usr/local` convention, both the
/// versioned directory and the `cuda` symlink the toolkit maintains.
fn default_cuda_roots() -> Vec<PathBuf> {
    if cfg!(windows) {
        let program_files = env::var_os("ProgramFiles")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("C:\\Program Files"));
        let toolkit = program_files
            .join("NVIDIA GPU Computing Toolkit")
            .join("CUDA");
        vec![toolkit.join(format!("v{CUDA_VERSION}"))]
    } else {
        vec![
            PathBuf::from("/usr/local").join(format!("cuda-{CUDA_VERSION}")),
            PathBuf::from("/usr/local").join("cuda"),
        ]
    }
}

fn nvcc_exe_name() -> &'static str {
    if cfg!(windows) { "nvcc.exe" } else { "nvcc" }
}

fn locate_cuda_host_compiler() -> Option<PathBuf> {
    if !cfg!(windows) {
        return None;
    }

    if let Some(path) = env::var_os(CUDA_CCBIN_ENV) {
        let ccbin = normalize_ccbin(PathBuf::from(path)).unwrap_or_else(|| {
            panic!(
                "CALYX_FORGE_CUDA_CCBIN_INVALID: {CUDA_CCBIN_ENV} must point to cl.exe or a directory containing cl.exe"
            )
        });
        println!(
            "cargo:warning=using CUDA host compiler from {CUDA_CCBIN_ENV}: {}",
            ccbin.display()
        );
        return Some(ccbin);
    }

    if command_on_path(cl_exe_name()) {
        println!("cargo:warning=using CUDA host compiler from PATH");
        return None;
    }

    let mut candidates = windows_msvc_ccbin_candidates();
    candidates.sort_by(|left, right| {
        msvc_version_key(left)
            .cmp(&msvc_version_key(right))
            .then_with(|| left.to_string_lossy().cmp(&right.to_string_lossy()))
    });
    candidates.dedup();
    let ccbin = candidates.pop().unwrap_or_else(|| {
        panic!(
            "CALYX_FORGE_CUDA_HOST_COMPILER_MISSING: nvcc requires cl.exe on Windows; install Visual Studio Build Tools with MSVC x64 tools or set {CUDA_CCBIN_ENV} to a Hostx64\\x64 directory"
        )
    });
    println!(
        "cargo:warning=using discovered CUDA host compiler: {}",
        ccbin.display()
    );
    Some(ccbin)
}

fn normalize_ccbin(path: PathBuf) -> Option<PathBuf> {
    if path.is_dir() && path.join(cl_exe_name()).is_file() {
        return Some(path);
    }
    if path.is_file()
        && path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.eq_ignore_ascii_case(cl_exe_name()))
    {
        return path.parent().map(Path::to_path_buf);
    }
    None
}

fn command_on_path(command: &str) -> bool {
    let Some(path) = env::var_os("PATH") else {
        return false;
    };
    env::split_paths(&path).any(|dir| dir.join(command).is_file())
}

fn cl_exe_name() -> &'static str {
    "cl.exe"
}

fn windows_msvc_ccbin_candidates() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(program_files) = env::var_os("ProgramFiles") {
        roots.push(PathBuf::from(program_files).join("Microsoft Visual Studio"));
    }
    if let Some(program_files_x86) = env::var_os("ProgramFiles(x86)") {
        roots.push(PathBuf::from(program_files_x86).join("Microsoft Visual Studio"));
    }

    let mut candidates = Vec::new();
    for root in roots {
        let Ok(major_dirs) = std::fs::read_dir(&root) else {
            continue;
        };
        for major_dir in major_dirs.flatten() {
            let Ok(edition_dirs) = std::fs::read_dir(major_dir.path()) else {
                continue;
            };
            for edition_dir in edition_dirs.flatten() {
                let msvc_root = edition_dir.path().join("VC").join("Tools").join("MSVC");
                let Ok(version_dirs) = std::fs::read_dir(&msvc_root) else {
                    continue;
                };
                for version_dir in version_dirs.flatten() {
                    let ccbin = version_dir.path().join("bin").join("Hostx64").join("x64");
                    if ccbin.join(cl_exe_name()).is_file() {
                        candidates.push(ccbin);
                    }
                }
            }
        }
    }
    candidates
}

fn msvc_version_key(ccbin: &Path) -> Vec<u32> {
    ccbin
        .ancestors()
        .nth(3)
        .and_then(|version_dir| version_dir.file_name())
        .and_then(|version| version.to_str())
        .map(|version| {
            version
                .split('.')
                .map(|part| part.parse::<u32>().unwrap_or(0))
                .collect()
        })
        .unwrap_or_default()
}

fn warn_nvcc_version(nvcc: &Path) {
    let output = Command::new(nvcc)
        .arg("--version")
        .output()
        .unwrap_or_else(|err| panic!("failed to run {} --version: {err}", nvcc.display()));
    assert_success(nvcc, &["--version"], output);

    let stdout = String::from_utf8_lossy(
        &Command::new(nvcc)
            .arg("--version")
            .output()
            .expect("rerun nvcc --version")
            .stdout,
    )
    .to_string();
    let summary = stdout
        .lines()
        .find(|line| line.contains("release") || line.contains("V13.3"))
        .unwrap_or_else(|| stdout.lines().next().unwrap_or("unknown nvcc version"));
    println!("cargo:warning=nvcc detected: {summary}");
}

fn compile_ptx(nvcc: &Path, host_compiler: Option<&Path>, src: &Path, out: &Path) {
    let args = deterministic_args(src, out, "--ptx", host_compiler);
    let output = Command::new(nvcc)
        .args(&args)
        .output()
        .unwrap_or_else(|err| panic!("failed to run {} for PTX: {err}", nvcc.display()));
    assert_success(nvcc, &args, output);
}

fn compile_cubin(nvcc: &Path, host_compiler: Option<&Path>, src: &Path, out: &Path) {
    let args = deterministic_args(src, out, "-cubin", host_compiler);
    let output = Command::new(nvcc)
        .args(&args)
        .output()
        .unwrap_or_else(|err| panic!("failed to run {} for cubin: {err}", nvcc.display()));
    assert_success(nvcc, &args, output);
}

fn deterministic_args(
    src: &Path,
    out: &Path,
    output_kind: &str,
    host_compiler: Option<&Path>,
) -> Vec<String> {
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
        output_kind.to_string(),
        "-o".to_string(),
        out.display().to_string(),
        src.display().to_string(),
    ]);
    args
}

fn assert_source_exists(src: &Path) {
    if !src.is_file() {
        panic!("CUDA kernel source not found: {}", src.display());
    }
}

fn assert_success(nvcc: &Path, args: &[impl AsRef<str>], output: Output) {
    if output.status.success() {
        return;
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let joined_args = args
        .iter()
        .map(|arg| arg.as_ref())
        .collect::<Vec<_>>()
        .join(" ");
    panic!(
        "nvcc command failed: {} {}\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
        nvcc.display(),
        joined_args,
        output.status,
        stdout,
        stderr
    );
}
