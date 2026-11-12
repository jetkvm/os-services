use std::{env, path::PathBuf};

#[path = "xtask/src/vcpkg_layout.rs"]
mod vcpkg_layout;

fn main() {
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os == "macos" {
        let sources = [
            "src/macos/event_loop.m",
            "src/macos/message.m",
            "src/macos/pasteboard.m",
        ];
        cc::Build::new().files(sources).compile("macos_helpers");
        let capture = "src/macos/screen_capture.m";
        cc::Build::new()
            .file(capture)
            .flag("-fobjc-arc")
            .compile("macos_screen_capture");
        let frameworks = [
            "AppKit",
            "CoreFoundation",
            "CoreMedia",
            "CoreVideo",
            "ScreenCaptureKit",
            "VideoToolbox",
        ];
        for framework in frameworks {
            println!("cargo:rustc-link-lib=framework={framework}");
        }
        for source in sources.iter().chain([&capture]) {
            println!("cargo:rerun-if-changed={source}");
        }
    }
    if target_os == "windows" {
        // The manifest declares per-monitor DPI awareness. Without it Windows
        // scales what the process sees of the desktop to the system DPI.
        let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("packaging")
            .join("windows")
            .join("app.manifest");
        println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
        println!(
            "cargo:rustc-link-arg-bins=/MANIFESTINPUT:{}",
            manifest.display()
        );
        println!("cargo:rerun-if-changed=packaging/windows/app.manifest");
    }
    build_ffmpeg_users(&target_os);
}

/// Compiles the native code that calls FFmpeg, and links FFmpeg from the
/// vcpkg tree that `cargo xtask setup` installs: the KMS capture on Linux,
/// the encoder wrapper on Windows and macOS.
fn build_ffmpeg_users(target_os: &str) {
    let installed = vcpkg_installed(target_os);
    println!(
        "cargo:rustc-link-search=native={}",
        installed.join("lib").display()
    );
    let mut build = cc::Build::new();
    build.include(installed.join("include"));
    let (ffmpeg, system) = match target_os {
        "linux" => build_kms_capture(build),
        _ => build_encoder(build, target_os),
    };
    for library in ffmpeg {
        println!("cargo:rustc-link-lib=static={library}");
        // Cargo does not track the libraries it links: a new build of the
        // vcpkg tree must run this script again, so the sender links it.
        for file in [format!("lib{library}.a"), format!("{library}.lib")] {
            let path = installed.join("lib").join(file);
            if path.exists() {
                println!("cargo:rerun-if-changed={}", path.display());
            }
        }
    }
    for library in system {
        println!("cargo:rustc-link-lib={library}");
    }
}

/// The installed tree of the target's triplet, where `cargo xtask setup`
/// puts it.
fn vcpkg_installed(target_os: &str) -> PathBuf {
    println!("cargo:rerun-if-env-changed=VCPKG_ROOT");
    println!("cargo:rerun-if-changed=xtask/src/vcpkg_layout.rs");
    let repository = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    vcpkg_layout::root(&repository, target_os)
        .join("installed")
        .join(vcpkg_layout::triplet(target_os))
}

type Libraries = (&'static [&'static str], &'static [&'static str]);

/// The Linux KMS capture, and the FFmpeg and system libraries it links.
/// VAAPI, EGL, GLES and libdrm come from the computer, because they must
/// match its graphics driver.
fn build_kms_capture(mut build: cc::Build) -> Libraries {
    let native = "src/video/kms/native";
    println!("cargo:rerun-if-changed={native}");
    build
        .cpp(true)
        // drm_fourcc.h for the scanout formats the PRIME frames carry.
        .include("/usr/include/libdrm")
        .files(
            ["kms_capture.cpp", "kms_vaapi.cpp", "kms_nvenc.cpp"]
                .map(|file| format!("{native}/{file}")),
        )
        .compile("kms_capture");
    (
        &["avfilter", "avcodec", "avutil"],
        &["va", "va-drm", "EGL", "GLESv2", "drm", "m"],
    )
}

/// The encoder wrapper of Windows and macOS, and the libraries it links.
fn build_encoder(mut build: cc::Build, target_os: &str) -> Libraries {
    let source = "src/video/ram/encoder.c";
    println!("cargo:rerun-if-changed={source}");
    build.file(source);
    if target_os == "windows" {
        build.static_crt(true).compile("ffmpeg_encoder");
        return (
            &["avcodec", "avutil", "libmfx"],
            &["user32", "bcrypt", "ole32", "advapi32", "d3d11", "dxgi"],
        );
    }
    build.compile("ffmpeg_encoder");
    (&["avcodec", "avutil"], &["m"])
}
