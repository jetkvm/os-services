//! Installers for the platform this runs on, checked and checksummed into
//! dist/. Windows gets an NSIS installer, macOS an app bundle and a DMG, and
//! Linux an AppImage.

use crate::{native, notices, process, root, Options, Result};
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

const PRODUCT: &str = "IP-KVM OS Services";
const EXECUTABLE: &str = "ipkvm-os-services";

pub fn package(options: Options) -> Result {
    crate::build::build()?;
    let dist = root().join("dist");
    if dist.exists() {
        fs::remove_dir_all(&dist).map_err(|error| format!("could not empty dist/: {error}"))?;
    }
    fs::create_dir_all(&dist).map_err(|error| error.to_string())?;
    if cfg!(windows) {
        windows(&dist)?;
    } else if cfg!(target_os = "macos") {
        macos(&dist, options.skip_notarization)?;
    } else {
        linux(&dist)?;
    }
    write_checksums(&dist)
}

/// Runs the Linux ci command in the build container defined by the
/// Dockerfile, and copies its dist/ out.
pub fn docker() -> Result {
    process::run(
        Command::new("docker")
            .args([
                "build",
                "--target",
                "dist",
                "--output",
                "type=local,dest=dist",
                ".",
            ])
            .current_dir(root()),
    )
}

fn bundle_dir() -> PathBuf {
    root().join("target").join("release").join("bundle")
}

/// The name of the notices file in each installer.
const NOTICES: &str = "THIRD_PARTY_NOTICES.md";

/// Writes the notices that tauri.conf.json bundles into the Windows
/// installer and the macOS app.
fn vcpkg_notices() -> Result {
    let installed = native::vcpkg_root().join("installed");
    notices::write(
        &root().join("target").join("notices").join(NOTICES),
        notices::Native::Vcpkg {
            installed: &installed,
            triplet: native::triplet(),
        },
    )
}

/// Runs the Tauri bundler for these formats. The bundle directory keeps
/// installers from earlier builds, so their directories are emptied first.
fn tauri_bundle(formats: &[&str]) -> Result {
    for format in formats {
        let dir = bundle_dir().join(if *format == "app" { "macos" } else { format });
        if dir.exists() {
            fs::remove_dir_all(&dir).map_err(|error| format!("{}: {error}", dir.display()))?;
        }
    }
    let mut command = process::cargo();
    command.args(["tauri", "bundle", "--bundles", &formats.join(",")]);
    if cfg!(windows) {
        command.arg("--no-sign");
    }
    // CI passes the signing secrets empty to a build that is not signed, and
    // Tauri reads an empty variable as set, so an empty one is removed.
    let set = |name: &str| std::env::var_os(name).is_some_and(|value| !value.is_empty());
    for name in [
        "APPLE_CERTIFICATE",
        "APPLE_CERTIFICATE_PASSWORD",
        "APPLE_SIGNING_IDENTITY",
    ] {
        if !set(name) {
            command.env_remove(name);
        }
    }
    if cfg!(target_os = "macos") && !set("APPLE_SIGNING_IDENTITY") {
        // An ad-hoc signature; CI passes the Developer ID certificate and
        // identity to a build of main and to a release, and Tauri imports
        // the certificate itself.
        command.env("APPLE_SIGNING_IDENTITY", "-");
    }
    // Tauri notarizes whenever these are set, even to empty values;
    // notarize() handles notarization for the DMG and the app.
    for name in ["APPLE_ID", "APPLE_PASSWORD", "APPLE_TEAM_ID"] {
        command.env_remove(name);
    }
    process::run(&mut command)
}

fn windows(dist: &Path) -> Result {
    vcpkg_notices()?;
    tauri_bundle(&["nsis"])?;
    smoke_test(&mut Command::new(
        root()
            .join("target/release")
            .join(format!("{EXECUTABLE}.exe")),
    ))?;
    copy_into(dist, &only_file(&bundle_dir().join("nsis"), "exe")?)
}

fn macos(dist: &Path, skip_notarization: bool) -> Result {
    vcpkg_notices()?;
    tauri_bundle(&["app", "dmg"])?;
    let app = bundle_dir().join("macos").join(format!("{PRODUCT}.app"));
    let dmg = only_file(&bundle_dir().join("dmg"), "dmg")?;
    check_app(&app)?;
    process::run(
        Command::new("codesign")
            .args(["--verify", "--deep", "--strict"])
            .arg(&app),
    )?;
    // Once the app has run, macOS stops processes without the App Management
    // permission, such as the CI runner, from writing into its bundle, so
    // stapler cannot add the ticket. The smoke test runs after notarization.
    notarize(&app, &dmg, skip_notarization)?;
    smoke_test(&mut Command::new(
        app.join("Contents/MacOS").join(EXECUTABLE),
    ))?;
    let archive = dist.join(format!("{}_arm64.app.tar.gz", PRODUCT.replace(' ', "-")));
    process::run(
        Command::new("tar")
            .arg("-czf")
            .arg(&archive)
            .arg("-C")
            .arg(app.parent().expect("the app is in a directory"))
            .arg(format!("{PRODUCT}.app")),
    )?;
    copy_into(dist, &dmg)?;
    process::run(
        Command::new("hdiutil")
            .arg("verify")
            .arg(dist.join(dmg.file_name().expect("a file"))),
    )
}

/// The bundle runs as a background agent, carries the configured
/// identifier and contains the notices.
fn check_app(app: &Path) -> Result {
    if !app.join("Contents/Resources").join(NOTICES).exists() {
        return Err(format!("the app bundle lacks Contents/Resources/{NOTICES}"));
    }
    let plist = app.join("Contents/Info.plist");
    let read = |key: &str| {
        process::output(
            Command::new("plutil")
                .args(["-extract", key, "raw", "-o", "-"])
                .arg(&plist),
        )
        .map(|value| value.trim().to_owned())
    };
    if read("LSUIElement")? != "true" {
        return Err("the app bundle must run without a Dock icon (LSUIElement)".into());
    }
    let config: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(root().join("tauri.conf.json")).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;
    if config["identifier"].as_str() != Some(read("CFBundleIdentifier")?.as_str()) {
        return Err("the app bundle identifier differs from tauri.conf.json".into());
    }
    Ok(())
}

/// Whether a build is notarized, from the number of the notarization
/// credentials APPLE_ID, APPLE_PASSWORD and APPLE_TEAM_ID that are set, the
/// kind of its signature, `--skip-notarization` and the Git ref CI builds.
///
/// Gatekeeper blocks a downloaded Developer ID app that is not notarized, so
/// a Developer ID signature without the credentials is an error unless
/// `--skip-notarization` says that the build is a test build. A test build
/// is signed with the Developer ID certificate because macOS then keeps its
/// privacy permissions from one build to the next. The build of a version
/// tag is a release, which `--skip-notarization` does not apply to. Ad-hoc
/// and self-signed builds are for development and are not notarized.
fn notarized(
    credentials: usize,
    developer_id: bool,
    skip_notarization: bool,
    git_ref: Option<&str>,
) -> Result<bool> {
    if skip_notarization {
        if git_ref.is_some_and(|git_ref| git_ref.starts_with("refs/tags/v")) {
            return Err(
                "--skip-notarization is for test builds; the build of a version tag \
                 is a release and is notarized"
                    .into(),
            );
        }
        return Ok(false);
    }
    match credentials {
        3 => Ok(true),
        0 if developer_id => Err("a Developer ID certificate signs the app, so APPLE_ID, \
             APPLE_PASSWORD and APPLE_TEAM_ID must be set to notarize it; \
             --skip-notarization leaves a test build without notarization"
            .into()),
        0 => Ok(false),
        _ => Err("APPLE_ID, APPLE_PASSWORD and APPLE_TEAM_ID must be set together".into()),
    }
}

/// Notarizes and staples the DMG and the app when notarized() says so.
fn notarize(app: &Path, dmg: &Path, skip_notarization: bool) -> Result {
    let credentials: Vec<String> = ["APPLE_ID", "APPLE_PASSWORD", "APPLE_TEAM_ID"]
        .iter()
        .filter_map(|name| std::env::var(name).ok().filter(|value| !value.is_empty()))
        .collect();
    // The signature decides, because APPLE_SIGNING_IDENTITY can name the
    // certificate by its hash. codesign prints the chain to stderr.
    let details = Command::new("codesign")
        .arg("-dvv")
        .arg(app)
        .output()
        .map_err(|error| format!("could not run codesign: {error}"))?;
    let developer_id =
        String::from_utf8_lossy(&details.stderr).contains("Authority=Developer ID Application");
    let git_ref = std::env::var("GITHUB_REF").ok();
    if !notarized(
        credentials.len(),
        developer_id,
        skip_notarization,
        git_ref.as_deref(),
    )? {
        println!("The build is not notarized");
        return Ok(());
    }
    let [id, password, team] = credentials.as_slice() else {
        unreachable!("notarized() accepts only all three credentials");
    };
    process::run(
        Command::new("xcrun")
            .args(["notarytool", "submit"])
            .arg(dmg)
            .args([
                "--apple-id",
                id,
                "--password",
                password,
                "--team-id",
                team,
                "--wait",
            ]),
    )?;
    process::run(Command::new("xcrun").args(["stapler", "staple"]).arg(dmg))?;
    process::run(Command::new("xcrun").args(["stapler", "staple"]).arg(app))
}

fn linux(dist: &Path) -> Result {
    let dir = bundle_dir().join("appimage");
    if dir.exists() {
        fs::remove_dir_all(&dir).map_err(|error| format!("{}: {error}", dir.display()))?;
    }
    let script = root().join("packaging/linux/package.sh");
    process::run(Command::new(&script).arg("deploy"))?;
    let appdir = root().join("target").join("release").join("appdir");
    notices::write(
        &appdir.join(DOC).join(NOTICES),
        notices::Native::AppImage {
            appdir: &appdir,
            installed: &native::vcpkg_root().join("installed"),
            triplet: native::triplet(),
        },
    )?;
    process::run(Command::new(&script).arg("appimage"))?;
    let appimage = only_file(&dir, "AppImage")?;
    check_appimage(&appimage)?;
    copy_into(dist, &appimage)
}

/// Where the AppImage keeps the notices.
const DOC: &str = "usr/share/doc/ipkvm-os-services";

/// The AppImage must carry the capture helper and its library at the paths
/// the sender looks in; without them the sender installs but cannot capture.
/// It must also carry the notices.
const BUNDLED: [&str; 3] = [
    "usr/lib/ipkvm-os-services/drmtap-helper",
    "usr/lib/ipkvm-os-services/libdrmtap.so.0",
    "usr/share/doc/ipkvm-os-services/THIRD_PARTY_NOTICES.md",
];

fn check_appimage(appimage: &Path) -> Result {
    let mut run = Command::new(appimage);
    run.env("APPIMAGE_EXTRACT_AND_RUN", "1");
    smoke_test(&mut run)?;
    let scratch = root().join("target").join("appimage-check");
    let _ = fs::remove_dir_all(&scratch);
    fs::create_dir_all(&scratch).map_err(|error| error.to_string())?;
    process::output(
        Command::new(appimage)
            .arg("--appimage-extract")
            .current_dir(&scratch),
    )?;
    let extracted = scratch.join("squashfs-root");
    match BUNDLED.iter().find(|file| !extracted.join(file).exists()) {
        Some(missing) => Err(format!("{} lacks {missing}", appimage.display())),
        None => Ok(()),
    }
}

/// The built program starts and rejects an unknown option with its usage.
fn smoke_test(program: &mut Command) -> Result {
    let output = program
        .args(["--invalid", "value"])
        .output()
        .map_err(|error| format!("could not run the packaged program: {error}"))?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    if output.status.success() || !stderr.contains("usage:") {
        return Err(format!(
            "the packaged program did not print its usage: {stderr}"
        ));
    }
    Ok(())
}

/// The one file with this extension in `dir`.
fn only_file(dir: &Path, extension: &str) -> Result<PathBuf> {
    let entries = fs::read_dir(dir).map_err(|error| format!("{}: {error}", dir.display()))?;
    let matching: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().is_some_and(|ext| ext == extension))
        .collect();
    match matching.as_slice() {
        [file] => Ok(file.clone()),
        _ => Err(format!(
            "expected one .{extension} in {}, found {}",
            dir.display(),
            matching.len()
        )),
    }
}

fn copy_into(dist: &Path, file: &Path) -> Result {
    let name = file.file_name().ok_or("a file name")?;
    fs::copy(file, dist.join(name)).map_err(|error| format!("{}: {error}", file.display()))?;
    Ok(())
}

/// SHA256SUMS in the format `sha256sum --check` reads.
fn write_checksums(dist: &Path) -> Result {
    let mut names: Vec<String> = fs::read_dir(dist)
        .map_err(|error| error.to_string())?
        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
        .collect();
    names.sort();
    let mut sums = String::new();
    for name in names {
        let bytes = fs::read(dist.join(&name)).map_err(|error| error.to_string())?;
        sums.push_str(&format!("{}  {name}\n", hex(&Sha256::digest(&bytes))));
    }
    fs::write(dist.join("SHA256SUMS"), sums).map_err(|error| error.to_string())
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const TAG: Option<&str> = Some("refs/tags/v0.4.5");
    const BRANCH: Option<&str> = Some("refs/heads/main");

    #[test]
    fn a_developer_id_build_is_notarized_unless_it_is_a_test_build() {
        assert_eq!(notarized(3, true, false, TAG), Ok(true));
        assert!(notarized(0, true, false, TAG).is_err());
        assert!(notarized(0, true, false, BRANCH).is_err());
        assert!(notarized(0, true, false, None).is_err());
        assert_eq!(notarized(0, true, true, BRANCH), Ok(false));
        assert_eq!(notarized(0, true, true, None), Ok(false));
    }

    #[test]
    fn a_version_tag_does_not_skip_notarization() {
        assert!(notarized(0, true, true, TAG).is_err());
        assert!(notarized(3, true, true, TAG).is_err());
        assert!(notarized(0, true, true, Some("refs/tags/v0.4.5-dev202610121200")).is_err());
    }

    #[test]
    fn an_ad_hoc_build_is_not_notarized_without_credentials() {
        assert_eq!(notarized(0, false, false, BRANCH), Ok(false));
        assert_eq!(notarized(0, false, true, BRANCH), Ok(false));
    }

    #[test]
    fn some_but_not_all_credentials_are_an_error() {
        assert!(notarized(1, false, false, BRANCH).is_err());
        assert!(notarized(2, true, false, TAG).is_err());
    }

    #[test]
    fn checksums_use_the_sha256sum_format() {
        let dist = std::env::temp_dir().join(format!("xtask-sums-{}", std::process::id()));
        fs::create_dir_all(&dist).unwrap();
        fs::write(dist.join("b.txt"), "abc").unwrap();
        fs::write(dist.join("a.txt"), "").unwrap();
        write_checksums(&dist).unwrap();
        let sums = fs::read_to_string(dist.join("SHA256SUMS")).unwrap();
        fs::remove_dir_all(&dist).unwrap();
        assert_eq!(
            sums,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855  a.txt\n\
             ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad  b.txt\n"
        );
    }
}
