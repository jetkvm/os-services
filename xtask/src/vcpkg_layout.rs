//! Where `cargo xtask setup` installs the vcpkg libraries. The service's
//! build.rs links them from there and includes this file too.

use std::path::{Path, PathBuf};

/// The vcpkg checkout: VCPKG_ROOT, or C:\vcpkg on Windows, where short paths
/// keep FFmpeg's build under the Windows path length limit, and target/vcpkg
/// in the repository elsewhere.
pub fn root(repository: &Path, target_os: &str) -> PathBuf {
    match std::env::var_os("VCPKG_ROOT") {
        Some(path) => path.into(),
        None if target_os == "windows" => PathBuf::from(r"C:\vcpkg"),
        None => repository.join("target").join("vcpkg"),
    }
}

/// The vcpkg triplet of a target system, as `std::env::consts::OS` and
/// CARGO_CFG_TARGET_OS name it.
pub fn triplet(target_os: &str) -> &'static str {
    match target_os {
        "windows" => "x64-windows-static",
        "linux" => "x64-linux",
        _ => "arm64-osx",
    }
}
