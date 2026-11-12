//! The native libraries the sender links, built with vcpkg from the
//! manifest in vendor/vcpkg. Linux also stages libdrmtap, and links the
//! computer's libva, libdrm and EGL, which must match its graphics driver.

use crate::{process, root, vcpkg_layout, Result};
use sha2::{Digest, Sha256};
use std::{path::PathBuf, process::Command};

/// The Tauri CLI version that bundles the Windows and macOS installers.
const TAURI_CLI_VERSION: &str = "2.11.4";

pub fn setup() -> Result {
    install_vcpkg()?;
    install_libraries()?;
    if cfg!(target_os = "linux") {
        return process::run(
            Command::new("packaging/libdrmtap/build.sh")
                .args(["--stage", "packaging/linux/drmtap"])
                .current_dir(root()),
        );
    }
    install_tauri_cli()
}

pub fn vcpkg_root() -> PathBuf {
    vcpkg_layout::root(&root(), std::env::consts::OS)
}

pub fn triplet() -> &'static str {
    vcpkg_layout::triplet(std::env::consts::OS)
}

/// The vcpkg commit the manifest pins.
fn baseline() -> Result<String> {
    let path = root().join("vendor").join("vcpkg").join("vcpkg.json");
    let text =
        std::fs::read_to_string(&path).map_err(|error| format!("{}: {error}", path.display()))?;
    let manifest: serde_json::Value =
        serde_json::from_str(&text).map_err(|error| error.to_string())?;
    manifest["builtin-baseline"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| "vcpkg.json has no builtin-baseline".into())
}

/// Checks out vcpkg at the pinned commit and builds its tool.
fn install_vcpkg() -> Result {
    let vcpkg = vcpkg_root();
    let baseline = baseline()?;
    if !vcpkg.join(".git").exists() {
        process::run(
            Command::new("git")
                .args(["clone", "https://github.com/microsoft/vcpkg"])
                .arg(&vcpkg),
        )?;
    }
    let git = || {
        let mut command = Command::new("git");
        command.arg("-C").arg(&vcpkg);
        command
    };
    let head = process::output(git().args(["rev-parse", "HEAD"])).unwrap_or_default();
    if head.trim() != baseline {
        let known = git()
            .args(["cat-file", "-e", &format!("{baseline}^{{commit}}")])
            .output();
        if !known.is_ok_and(|output| output.status.success()) {
            process::run(git().args(["fetch", "origin", &baseline]))?;
        }
        process::run(git().args(["checkout", "--quiet", "--detach", &baseline]))?;
    }
    let bootstrap = if cfg!(windows) {
        "bootstrap-vcpkg.bat"
    } else {
        "bootstrap-vcpkg.sh"
    };
    process::run(Command::new(vcpkg.join(bootstrap)).arg("-disableMetrics"))
}

/// Builds the manifest's libraries into VCPKG_ROOT/installed, where the
/// build scripts look for them. VCPKG_BINARY_SOURCES selects a binary
/// cache.
fn install_libraries() -> Result {
    let vcpkg = vcpkg_root();
    let tool = vcpkg.join(if cfg!(windows) { "vcpkg.exe" } else { "vcpkg" });
    process::run(
        Command::new(tool)
            .arg("install")
            .args(["--triplet", triplet(), "--host-triplet", triplet()])
            .arg(format!(
                "--x-manifest-root={}",
                root().join("vendor").join("vcpkg").display()
            ))
            .arg(format!(
                "--x-install-root={}",
                vcpkg.join("installed").display()
            )),
    )
}

/// The Tauri release archive for this computer and its SHA-256.
fn tauri_cli_archive() -> (&'static str, &'static str) {
    if cfg!(windows) {
        (
            "cargo-tauri-x86_64-pc-windows-msvc.zip",
            "0743e30a661a35d63339b24cf63828f97ba5389a1d7f13b368a542794dd0a3f3",
        )
    } else {
        (
            "cargo-tauri-aarch64-apple-darwin.zip",
            "82bdcb9ae7f407882321680ae50750f11623fae22445f8b00b096e10f815d604",
        )
    }
}

/// Puts the prebuilt cargo-tauri from the Tauri release into Cargo's bin
/// directory, where `cargo tauri` finds it. Compiling it takes about 18
/// minutes on the Windows runner.
fn install_tauri_cli() -> Result {
    let wanted = format!("tauri-cli {TAURI_CLI_VERSION}");
    let installed =
        process::output(process::cargo().args(["tauri", "--version"])).unwrap_or_default();
    if installed.trim() == wanted {
        return Ok(());
    }
    let (name, sha256) = tauri_cli_archive();
    let download = root().join("target").join("tauri-cli");
    std::fs::create_dir_all(&download).map_err(|error| error.to_string())?;
    let archive = download.join(name);
    process::run(
        Command::new("curl")
            .args(["--fail", "--silent", "--show-error", "--location", "--output"])
            .arg(&archive)
            .arg(format!(
                "https://github.com/tauri-apps/tauri/releases/download/tauri-cli-v{TAURI_CLI_VERSION}/{name}"
            )),
    )?;
    let bytes = std::fs::read(&archive).map_err(|error| error.to_string())?;
    if crate::package::hex(&Sha256::digest(&bytes)) != sha256 {
        return Err(format!("{name} does not have the expected SHA-256"));
    }
    let bin = cargo_home().join("bin");
    std::fs::create_dir_all(&bin).map_err(|error| error.to_string())?;
    // Windows' own tar reads zip files; the GNU tar of Git for Windows does not.
    let tar = if cfg!(windows) {
        PathBuf::from(std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into()))
            .join("System32")
            .join("tar.exe")
    } else {
        PathBuf::from("tar")
    };
    let binary = if cfg!(windows) {
        "cargo-tauri.exe"
    } else {
        "cargo-tauri"
    };
    process::run(
        Command::new(tar)
            .arg("-xf")
            .arg(&archive)
            .arg("-C")
            .arg(&bin)
            .arg(binary),
    )?;
    let installed = process::output(process::cargo().args(["tauri", "--version"]))?;
    if installed.trim() != wanted {
        return Err(format!(
            "cargo tauri reports {}, not {wanted}",
            installed.trim()
        ));
    }
    Ok(())
}

/// CARGO_HOME, or .cargo in the home directory.
fn cargo_home() -> PathBuf {
    if let Some(path) = std::env::var_os("CARGO_HOME") {
        return path.into();
    }
    let home = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    PathBuf::from(std::env::var_os(home).unwrap_or_default()).join(".cargo")
}
