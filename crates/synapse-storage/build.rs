use std::{
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
};

fn main() {
    let manifest_dir = match std::env::var_os("CARGO_MANIFEST_DIR") {
        Some(value) => PathBuf::from(value),
        None => return,
    };
    let forbidden = ["bin", "code"].concat();
    for path in [
        manifest_dir.join("Cargo.toml"),
        manifest_dir.join("build.rs"),
        manifest_dir.join("src"),
        manifest_dir.join("tests"),
        manifest_dir.join("benches"),
        manifest_dir.join("examples"),
    ] {
        scan_path(&path, &forbidden);
    }
}

fn scan_path(path: &Path, forbidden: &str) {
    println!("cargo:rerun-if-changed={}", path.display());
    if path.is_dir() {
        scan_dir(path, forbidden);
    } else if should_scan_file(path) {
        scan_file(path, forbidden);
    }
}

fn scan_dir(path: &Path, forbidden: &str) {
    let entries = match fs::read_dir(path) {
        Ok(entries) => entries,
        Err(error) => panic!("read_dir {} failed: {error}", path.display()),
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => panic!("read_dir entry under {} failed: {error}", path.display()),
        };
        let child = entry.path();
        if child.is_dir() {
            scan_dir(&child, forbidden);
        } else if should_scan_file(&child) {
            scan_file(&child, forbidden);
        }
    }
}

fn should_scan_file(path: &Path) -> bool {
    path.extension() == Some(OsStr::new("rs")) || path.file_name() == Some(OsStr::new("Cargo.toml"))
}

fn scan_file(path: &Path, forbidden: &str) {
    println!("cargo:rerun-if-changed={}", path.display());
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) => panic!("read {} failed: {error}", path.display()),
    };
    assert!(
        !text.contains(forbidden),
        "forbidden binary storage codec token found in {}",
        path.display()
    );
}
