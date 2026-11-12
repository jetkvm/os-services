//! Replacing the installed sender with a release package from the KVM. Each
//! OS has one form of the program that the desktop user can replace without
//! privileges: the per-user Windows installer, the macOS app bundle, and the
//! files of the Linux AppImage in the user's folder. Any other form, such as
//! a development build, offers no update.

use crate::protocol::UpdatePackage;
use std::{
    error::Error,
    fs::{self, File},
    io,
    path::{Path, PathBuf},
};

/// Where this sender is installed, when it can replace itself.
#[derive(Clone, Debug)]
pub struct Installation {
    package: UpdatePackage,
    /// The Windows install folder, the macOS app bundle, or on Linux the
    /// folder of the installation or an AppImage.
    target: PathBuf,
    /// Where the package is written before it is installed.
    staging: PathBuf,
}

/// A complete package whose hash matched, ready to install.
#[derive(Debug)]
pub struct Staged {
    installation: Installation,
    path: PathBuf,
}

impl Installation {
    /// This process's installation, or None when it cannot replace itself.
    pub fn current() -> Option<Self> {
        let (package, target) = platform::current()?;
        Some(Self {
            package,
            staging: platform::staging_path(&target),
            target,
        })
    }

    /// An installation that stages its package at `staging`.
    #[cfg(test)]
    pub fn for_test(staging: PathBuf) -> Self {
        Self {
            package: UpdatePackage::LinuxX86_64AppImage,
            target: staging.clone(),
            staging,
        }
    }

    /// The same installation with its program at `target`.
    #[cfg(all(test, target_os = "linux", target_arch = "x86_64"))]
    pub fn with_target(mut self, target: PathBuf) -> Self {
        self.target = target;
        self
    }

    pub fn package(&self) -> UpdatePackage {
        self.package
    }

    /// Creates the file the package is written to, replacing what an
    /// earlier update left there.
    pub fn create_staging_file(&self) -> io::Result<(File, PathBuf)> {
        if let Some(directory) = self.staging.parent() {
            fs::create_dir_all(directory)?;
        }
        Ok((File::create(&self.staging)?, self.staging.clone()))
    }

    pub fn stage(&self, path: PathBuf) -> Staged {
        Staged {
            installation: self.clone(),
            path,
        }
    }

    /// Checks that the package comes from the publisher of this
    /// installation. An installation that is not signed accepts every
    /// package.
    pub fn check_publisher(&self, package: &Path) -> Result<(), String> {
        platform::check_publisher(&self.target, package)
    }
}

/// The status the process exits with after an install, so that the service
/// manager starts the new program.
pub fn restart_status() -> i32 {
    #[cfg(target_os = "linux")]
    if super::keeper::kept() {
        return super::keeper::RESTART_STATUS;
    }
    platform::RESTART_STATUS
}

impl Staged {
    /// Installs the package. Ok means the caller exits the process at once
    /// with `restart_status`, and the new program starts in its place.
    pub fn install(self) -> Result<(), Box<dyn Error>> {
        eprintln!(
            "ipkvm-os-services: installing the update over {}",
            self.installation.target.display()
        );
        let result = platform::install(&self.installation.target, &self.path);
        if result.is_err() {
            discard(&self.path);
        }
        result
    }
}

/// Removes a staging file that will not be installed.
pub fn discard(path: &Path) {
    let _ = fs::remove_file(path);
}

/// The start of the program of a package, on the systems where the update
/// replaces the files of the program: Linux and macOS.
#[cfg(any(target_os = "linux", all(target_os = "macos", target_arch = "aarch64")))]
pub(super) mod start {
    use std::{
        error::Error,
        path::Path,
        process::{Command, Stdio},
        thread,
        time::{Duration, Instant},
    };

    /// How long the program of a package may take to print its version.
    pub const WITHIN: Duration = Duration::from_secs(10);

    /// Fails unless the program starts on this computer: `--version` must
    /// end successfully. A program that needs a library or a system
    /// function that the computer lacks ends before it runs. After the swap
    /// nothing puts the previous files back, so such a package is refused
    /// here.
    pub fn check(program: &Path, within: Duration) -> Result<(), Box<dyn Error>> {
        let mut started = Command::new(program)
            .arg(crate::supervisor::VERSION_FLAG)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| format!("the program in the package does not start: {error}"))?;
        let deadline = Instant::now() + within;
        loop {
            if let Some(status) = started.try_wait()? {
                return match status.success() {
                    true => Ok(()),
                    false => {
                        Err(format!("the program in the package does not start: {status}").into())
                    }
                };
            }
            if Instant::now() >= deadline {
                let _ = started.kill();
                let _ = started.wait();
                return Err("the program in the package does not end".into());
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    #[cfg(test)]
    mod tests {
        use super::check;
        use std::{fs, os::unix::fs::PermissionsExt, time::Duration};

        #[test]
        fn only_a_program_that_starts_passes() {
            let root = std::env::temp_dir().join(format!("starts-{}", std::process::id()));
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(&root).unwrap();
            let program = |name: &str, script: &str| {
                let path = root.join(name);
                fs::write(&path, format!("#!/bin/sh\n{script}\n")).unwrap();
                fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
                path
            };
            let within = Duration::from_secs(5);
            check(&program("good", r#"[ "$1" = --version ]"#), within).unwrap();
            // The status of a program that the loader could not start.
            check(&program("no-library", "exit 127"), within).unwrap_err();
            check(&root.join("missing"), within).unwrap_err();
            let hangs = check(&program("hangs", "sleep 30"), Duration::from_millis(200));
            assert!(hangs.unwrap_err().to_string().contains("does not end"));
            fs::remove_dir_all(&root).unwrap();
        }
    }
}

#[cfg(all(windows, target_arch = "x86_64"))]
mod platform {
    use super::{Error, Path, PathBuf, UpdatePackage};
    use crate::supervisor::authenticode::{self, Publisher};
    use std::{os::windows::process::CommandExt, process::Command};

    /// The Windows installer runs silently, without a console window, and
    /// outside the scheduled task's job so that it outlives the sender.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;

    /// The update script starts the scheduled task again.
    pub const RESTART_STATUS: i32 = 0;

    /// The per-user installer puts its uninstaller next to the program. A
    /// program without one was not installed by the installer.
    pub fn current() -> Option<(UpdatePackage, PathBuf)> {
        let folder = std::env::current_exe().ok()?.parent()?.to_path_buf();
        folder
            .join("uninstall.exe")
            .is_file()
            .then_some((UpdatePackage::WindowsX86_64, folder))
    }

    pub fn staging_path(_: &Path) -> PathBuf {
        std::env::temp_dir()
            .join("ipkvm-os-services-update")
            .join("IP-KVM-OS-Services-setup.exe")
    }

    /// The installer must have an Authenticode signature of the publisher
    /// that signed this program (see `authenticode`). A program without a
    /// signature, such as a development build, names no publisher.
    pub fn check_publisher(_: &Path, setup: &Path) -> Result<(), String> {
        let program = std::env::current_exe()
            .map_err(|error| format!("this service cannot find its own program: {error}"))?;
        if authenticode::check(&program, setup)? == Publisher::Unknown {
            eprintln!(
                "ipkvm-os-services: this program is not signed, so the update is not checked"
            );
        }
        Ok(())
    }

    /// The installer cannot replace a running program, so a script waits
    /// for this process to exit, runs the installer, and starts the
    /// scheduled task again.
    pub fn install(_: &Path, setup: &Path) -> Result<(), Box<dyn Error>> {
        let mut command = Command::new("powershell.exe");
        command
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                include_str!("../../packaging/windows/update.ps1"),
            ])
            .env("OS_SERVICES_SETUP", setup)
            .env("OS_SERVICES_PID", std::process::id().to_string())
            .env("OS_SERVICES_LOG", super::super::log_path()?);
        // A job that forbids breakaway refuses the flag; the script then
        // runs inside it, which holds while the job outlives the sender.
        command.creation_flags(CREATE_NO_WINDOW | CREATE_BREAKAWAY_FROM_JOB);
        if command.spawn().is_err() {
            command.creation_flags(CREATE_NO_WINDOW);
            command.spawn()?;
        }
        Ok(())
    }
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
mod platform {
    use super::{start, Error, Path, PathBuf, UpdatePackage};
    use crate::supervisor::{run, unit_files::LAUNCHD_LABEL};
    use core_foundation::{
        base::{CFType, TCFType},
        dictionary::{CFDictionary, CFDictionaryRef},
        string::{CFString, CFStringRef},
        url::CFURL,
    };
    use security_framework::os::macos::code_signing::{Flags, SecRequirement, SecStaticCode};
    use std::{ffi::CString, fs, io::Read, os::unix::ffi::OsStrExt};

    const STAGING_DIR: &str = ".ipkvm-os-services-update";

    /// kSecCSSigningInformation: SecCodeCopySigningInformation returns who
    /// signed the code.
    const SIGNING_INFORMATION: u32 = 1 << 1;

    #[link(name = "Security", kind = "framework")]
    extern "C" {
        static kSecCodeInfoTeamIdentifier: CFStringRef;

        fn SecCodeCopySigningInformation(
            code: <SecStaticCode as TCFType>::Ref,
            flags: u32,
            information: *mut CFDictionaryRef,
        ) -> i32;
    }

    /// The Mach-O CPU type of arm64.
    const ARM64: u32 = 0x0100_000c;

    /// macOS starts an arm64e program only when Apple built it, so a program
    /// with this subtype does not count as arm64. The top byte holds flags.
    fn arm64e(subtype: u32) -> bool {
        subtype & 0x00ff_ffff == 2
    }

    /// EX_TEMPFAIL: the launchd agent's KeepAlive starts the job again after
    /// an unsuccessful exit (see `unit_files::launchd_plist`), so it starts
    /// the new program. An exec in this process would not do: TCC checks
    /// the grant against the program launchd started, and exec keeps that
    /// program, the previous bundle, which the update removes. The new
    /// program would have no Screen Recording grant.
    pub const RESTART_STATUS: i32 = 75;

    /// The program runs from Contents/MacOS inside the app bundle. Only the
    /// agent's KeepAlive starts the new program after an update (see
    /// RESTART_STATUS), so a program that the agent did not start, such as
    /// one run with --run from a terminal, offers no update. An update that
    /// failed left its files in the staging folder, so the folder goes.
    pub fn current() -> Option<(UpdatePackage, PathBuf)> {
        if !started_by_agent() {
            return None;
        }
        let bundle = bundle()?;
        let _ = fs::remove_dir_all(staging_dir(&bundle));
        Some((UpdatePackage::MacosAarch64, bundle))
    }

    /// launchd names the agent's job in XPC_SERVICE_NAME and is its parent.
    /// A child of the sender, such as a terminal shell, inherits the name
    /// but not the parent.
    fn started_by_agent() -> bool {
        std::env::var_os("XPC_SERVICE_NAME").is_some_and(|job| job == LAUNCHD_LABEL)
            && unsafe { libc::getppid() } == 1
    }

    /// The app bundle this program runs from.
    fn bundle() -> Option<PathBuf> {
        let executable = std::env::current_exe().ok()?;
        let bundle = executable.parent()?.parent()?.parent()?;
        (bundle.extension()? == "app").then(|| bundle.to_path_buf())
    }

    /// Fails unless the program is a file, not a link, that this user can
    /// execute, and a thin arm64 Mach-O program, as the releases build it.
    /// After the swap nothing puts the previous bundle back, so a program
    /// that cannot start is refused here.
    pub(super) fn check_runnable(program: &Path) -> Result<(), Box<dyn Error>> {
        if !fs::symlink_metadata(program)?.is_file() {
            return Err(format!("{} is not a file", program.display()).into());
        }
        let name = CString::new(program.as_os_str().as_bytes())?;
        if unsafe { libc::access(name.as_ptr(), libc::X_OK) } != 0 {
            return Err(format!("{} is not executable", program.display()).into());
        }
        let mut head = Vec::new();
        fs::File::open(program)?.take(16).read_to_end(&mut head)?;
        if !arm64_program(&head) {
            return Err(format!("{} is not an arm64 program", program.display()).into());
        }
        Ok(())
    }

    /// Whether a thin 64-bit Mach-O header is an arm64 program: the magic
    /// number, the CPU, a subtype other than arm64e, and file type 2.
    fn arm64_program(header: &[u8]) -> bool {
        let word = |at: usize| {
            let bytes: [u8; 4] = header.get(at..at + 4)?.try_into().ok()?;
            Some(u32::from_le_bytes(bytes))
        };
        word(0) == Some(0xfeed_facf)
            && word(4) == Some(ARM64)
            && word(8).is_some_and(|subtype| !arm64e(subtype))
            && word(12) == Some(2)
    }

    /// The staging folder is next to the bundle, on the same volume, so the
    /// new bundle moves into place with a rename. A folder the user cannot
    /// write fails the update before any byte arrives.
    pub fn staging_path(bundle: &Path) -> PathBuf {
        staging_dir(bundle).join("IP-KVM-OS-Services.app.tar.gz")
    }

    fn staging_dir(bundle: &Path) -> PathBuf {
        bundle.with_file_name(STAGING_DIR)
    }

    /// Fails unless the app in the package has a valid code signature from
    /// the Developer ID team that signed the installed app. An installed app
    /// without a team, such as a development build, accepts every package.
    /// The app is unpacked for the check and removed after it; `install`
    /// unpacks the package again and checks the app that it puts in place.
    pub fn check_publisher(bundle: &Path, archive: &Path) -> Result<(), String> {
        if team(bundle).is_none() {
            return Ok(());
        }
        let work = archive.parent().ok_or("the staging file has no folder")?;
        let checked = work.join("checked");
        let _ = fs::remove_dir_all(&checked);
        let signed = fs::create_dir(&checked)
            .map_err(Box::<dyn Error>::from)
            .and_then(|()| extract(archive, &checked))
            .map_err(|error| format!("the package could not be unpacked: {error}"))
            .and_then(|fresh| check_same_team(bundle, &fresh));
        let _ = fs::remove_dir_all(&checked);
        signed
    }

    /// Fails unless the unpacked app has a valid code signature from the
    /// team that signed the installed app, when the installed app has one.
    fn check_same_team(bundle: &Path, fresh: &Path) -> Result<(), String> {
        let Some(team) = team(bundle) else {
            return Ok(());
        };
        require_team(fresh, &team).map_err(|reason| format!("the package is refused: {reason}"))
    }

    /// The Developer ID team that signed the app. An app with no signature,
    /// an ad hoc signature, or a certificate that Apple did not issue has
    /// none.
    pub(super) fn team(bundle: &Path) -> Option<String> {
        let code = static_code(bundle).ok()?;
        // The signing information comes from a validation of the signature,
        // which the framework does one time for each code object. This
        // validation is that one, and it runs without network access. Its
        // result does not matter here: a signature that is not valid gives
        // no team below.
        let always: SecRequirement = "always".parse().ok()?;
        let _ = code.check_validity(
            Flags::BASIC_VALIDATE_ONLY | Flags::NO_NETWORK_ACCESS,
            &always,
        );
        let mut information: CFDictionaryRef = std::ptr::null();
        let status = unsafe {
            SecCodeCopySigningInformation(
                code.as_concrete_TypeRef(),
                SIGNING_INFORMATION,
                &mut information,
            )
        };
        if status != 0 || information.is_null() {
            return None;
        }
        let information: CFDictionary<CFString, CFType> =
            unsafe { CFDictionary::wrap_under_create_rule(information) };
        let key = unsafe { CFString::wrap_under_get_rule(kSecCodeInfoTeamIdentifier) };
        let team = information.find(&key)?.downcast::<CFString>()?;
        Some(team.to_string())
    }

    /// Fails unless the app is intact and its signature leads from a
    /// certificate of the team to Apple's root. The check covers every
    /// architecture, the code inside the app and its resources, with the
    /// strict rules that `codesign --verify --deep --strict` applies. It
    /// reads the certificates in the signature and the Apple root on this
    /// computer; NO_NETWORK_ACCESS turns off every lookup on the network.
    pub(super) fn require_team(bundle: &Path, team: &str) -> Result<(), String> {
        // The team goes into the requirement text, so it must be plain.
        if team.is_empty() || !team.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
            return Err(format!("{team:?} is not a team identifier"));
        }
        let requirement: SecRequirement =
            format!("anchor apple generic and certificate leaf[subject.OU] = \"{team}\"")
                .parse()
                .map_err(|error| format!("{error}"))?;
        let flags = Flags::CHECK_ALL_ARCHITECTURES
            | Flags::CHECK_NESTED_CODE
            | Flags::STRICT_VALIDATE
            | Flags::RESTRICT_SYMLINKS
            | Flags::RESTRICT_SIDEBAND_DATA
            | Flags::NO_NETWORK_ACCESS;
        static_code(bundle)?
            .check_validity(flags, &requirement)
            .map_err(|error| format!("{error}"))
    }

    fn static_code(bundle: &Path) -> Result<SecStaticCode, String> {
        let url = CFURL::from_path(bundle, true)
            .ok_or_else(|| format!("{} is not a path", bundle.display()))?;
        SecStaticCode::from_path(&url, Flags::NONE).map_err(|error| format!("{error}"))
    }

    /// Unpacks the bundle, starts its program one time (see `start`), and
    /// swaps the bundle for the running one. The caller then exits with
    /// RESTART_STATUS, and launchd starts the new program. The program
    /// starts only after the publisher check, so an installed app with a
    /// team runs no code of another publisher.
    pub fn install(bundle: &Path, archive: &Path) -> Result<(), Box<dyn Error>> {
        let work = archive.parent().ok_or("the staging file has no folder")?;
        let unpacked = work.join("unpacked");
        let _ = fs::remove_dir_all(&unpacked);
        fs::create_dir(&unpacked)?;
        let swapped = unpack(archive, &unpacked).and_then(|fresh| {
            check_same_team(bundle, &fresh)?;
            start::check(&program(&fresh)?, start::WITHIN)?;
            swap(bundle, &fresh, work)
        });
        if swapped.is_err() {
            let _ = fs::remove_dir_all(&unpacked);
        }
        swapped?;
        // This program can keep running from the removed files until it
        // exits.
        let _ = fs::remove_dir_all(work);
        Ok(())
    }

    /// Moves the installed bundle into the staging folder and the new one
    /// into its place, or puts the installed one back.
    fn swap(bundle: &Path, fresh: &Path, work: &Path) -> Result<(), Box<dyn Error>> {
        // A swap that could not put the bundle back left the only copy in
        // the staging folder, so it must not be removed.
        if !bundle.exists() {
            return Err(format!("{} is missing", bundle.display()).into());
        }
        let previous = work.join("previous.app");
        let _ = fs::remove_dir_all(&previous);
        fs::rename(bundle, &previous)?;
        if let Err(error) = fs::rename(fresh, bundle) {
            let _ = fs::rename(&previous, bundle);
            return Err(error.into());
        }
        Ok(())
    }

    /// Unpacks the archive into `unpacked` and returns the bundle in it.
    fn extract(archive: &Path, unpacked: &Path) -> Result<PathBuf, Box<dyn Error>> {
        let path = |path: &Path| path.to_str().map(str::to_owned).ok_or("path is not UTF-8");
        run("tar", &["-xzf", &path(archive)?, "-C", &path(unpacked)?])?;
        // The archive holds one bundle; the installed one can have another
        // name. With two bundles, the publisher check and the install could
        // each take a different one.
        let mut bundles = fs::read_dir(unpacked)?
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|entry| {
                entry
                    .extension()
                    .is_some_and(|extension| extension == "app")
            });
        let fresh = bundles.next().ok_or("the package holds no app bundle")?;
        if bundles.next().is_some() {
            return Err("the package holds more than one app bundle".into());
        }
        Ok(fresh)
    }

    /// Unpacks the archive into `unpacked` and returns the bundle in it,
    /// once its program is found runnable.
    fn unpack(archive: &Path, unpacked: &Path) -> Result<PathBuf, Box<dyn Error>> {
        let fresh = extract(archive, unpacked)?;
        let program = program(&fresh)?;
        if !program.is_file() {
            return Err(format!("the package holds no {}", program.display()).into());
        }
        check_runnable(&program)?;
        Ok(fresh)
    }

    /// The program of a bundle, named as the running program is.
    fn program(bundle: &Path) -> Result<PathBuf, Box<dyn Error>> {
        let executable = std::env::current_exe()?;
        let name = executable.file_name().ok_or("the program has no name")?;
        Ok(bundle.join("Contents/MacOS").join(name))
    }
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
mod platform {
    use super::{Error, Path, PathBuf, UpdatePackage};
    use crate::supervisor::{installed, keeper, publisher, run, unit_files};
    use std::{fs, os::unix::fs::PermissionsExt};

    /// systemd restarts the unit on request (see `install`). A keeper has
    /// its own status (see `restart_status`).
    pub const RESTART_STATUS: i32 = 0;

    /// The program runs from its folder (see `installed`), or from an
    /// AppImage that could not move there. Any other program is a
    /// development build.
    pub fn current() -> Option<(UpdatePackage, PathBuf)> {
        let target =
            installed::current().or_else(|| std::env::var_os("APPIMAGE").map(PathBuf::from))?;
        Some((UpdatePackage::LinuxX86_64AppImage, target))
    }

    /// The staging file is in the folder. For an AppImage it is next to the
    /// image, so it replaces the image with a rename. A folder the user
    /// cannot write fails the update before any byte arrives.
    pub fn staging_path(target: &Path) -> PathBuf {
        if target.is_dir() {
            return installed::staging_path(target);
        }
        let name = target.file_name().unwrap_or_default().to_string_lossy();
        target.with_file_name(format!(".{name}.update"))
    }

    /// The installation keeps the key block of its release key, and the
    /// package must carry a signature from that key (see `publisher`). An
    /// installation without a key block is not a signed release, and it
    /// accepts every package. The check also covers the key block that
    /// `install` keeps, so that step does not fail after the service
    /// reported Installing.
    pub fn check_publisher(target: &Path, package: &Path) -> Result<(), String> {
        let own = own_publisher(target)?;
        if target.is_dir() {
            // The folder keeps the key block in a file of its own.
            return publisher::adopt(own.as_deref(), package).map(|_| ());
        }
        publisher::check(own.as_deref(), package)
    }

    /// The key block that the folder keeps, or the one in the AppImage.
    fn own_publisher(target: &Path) -> Result<Option<Vec<u8>>, String> {
        if target.is_dir() {
            return installed::publisher(target);
        }
        publisher::key_block(target)
    }

    /// Replaces the installed files, or the image, and asks systemd to
    /// restart the unit. The unit keeps its path, so it starts the new
    /// program. A keeper starts the program again when this process exits
    /// (see `restart_status`). A new image gets the key block that the installation keeps
    /// (see `publisher`), in place of the one that came with the package.
    pub fn install(target: &Path, staged: &Path) -> Result<(), Box<dyn Error>> {
        if target.is_dir() {
            installed::install_image(target, staged)?;
        } else {
            match publisher::adopt(own_publisher(target)?.as_deref(), staged)? {
                Some(kept) => publisher::set_key_block(staged, &kept)?,
                // A package that is not an AppImage has no key section.
                None => drop(publisher::set_key_block(staged, &[])),
            }
            fs::set_permissions(staged, fs::Permissions::from_mode(0o755))?;
            fs::rename(staged, target)?;
        }
        if keeper::kept() {
            return Ok(());
        }
        run(
            "systemctl",
            &["--user", "--no-block", "restart", unit_files::SYSTEMD_UNIT],
        )?;
        Ok(())
    }
}

/// A build with no release package for its OS and architecture.
#[cfg(not(any(
    all(windows, target_arch = "x86_64"),
    all(target_os = "macos", target_arch = "aarch64"),
    all(target_os = "linux", target_arch = "x86_64")
)))]
mod platform {
    use super::{Error, Path, PathBuf, UpdatePackage};

    pub const RESTART_STATUS: i32 = 0;

    pub fn current() -> Option<(UpdatePackage, PathBuf)> {
        None
    }

    pub fn staging_path(target: &Path) -> PathBuf {
        target.to_path_buf()
    }

    /// This platform has no publisher check.
    pub fn check_publisher(_: &Path, _: &Path) -> Result<(), String> {
        Ok(())
    }

    pub fn install(_: &Path, _: &Path) -> Result<(), Box<dyn Error>> {
        Err("this build has no release package".into())
    }
}

#[cfg(all(test, target_os = "macos", target_arch = "aarch64"))]
mod tests {
    use super::platform;
    use std::{
        fs,
        os::unix::fs::PermissionsExt,
        path::{Path, PathBuf},
        process::Command,
    };

    /// The first bytes of a thin arm64 Mach-O program: magic, CPU, subtype
    /// and file type.
    const THIN_ARM64: [u8; 16] = [
        0xcf, 0xfa, 0xed, 0xfe, 0x0c, 0, 0, 0x01, 0, 0, 0, 0, 0x02, 0, 0, 0,
    ];

    /// A temporary folder that is removed however the test ends.
    struct Root(PathBuf);

    impl Root {
        fn new(name: &str) -> Self {
            let root = std::env::temp_dir().join(format!("{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(&root).unwrap();
            Self(root)
        }
    }

    impl Drop for Root {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn write_program(path: &Path, bytes: &[u8], mode: u32) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }

    fn runnable(bytes: &[u8], mode: u32) -> bool {
        let root = Root::new("macos-runnable");
        let program = root.0.join("program");
        write_program(&program, bytes, mode);
        platform::check_runnable(&program).is_ok()
    }

    #[test]
    fn only_executable_arm64_programs_are_runnable() {
        assert!(runnable(&THIN_ARM64, 0o755));
        assert!(!runnable(&THIN_ARM64, 0o644));
        assert!(!runnable(
            &[0xcf, 0xfa, 0xed, 0xfe, 0x07, 0, 0, 0x01, 0x03, 0, 0, 0, 0x02, 0, 0, 0],
            0o755
        ));
        // arm64e, which macOS starts only for Apple's own programs.
        assert!(!runnable(
            &[0xcf, 0xfa, 0xed, 0xfe, 0x0c, 0, 0, 0x01, 0x02, 0, 0, 0x80, 0x02, 0, 0, 0],
            0o755
        ));
        // An arm64 library (file type 6) is not a program.
        assert!(!runnable(
            &[0xcf, 0xfa, 0xed, 0xfe, 0x0c, 0, 0, 0x01, 0, 0, 0, 0, 0x06, 0, 0, 0],
            0o755
        ));
        // This test program, which the toolchain built for arm64.
        let this = std::env::current_exe().unwrap();
        assert!(platform::check_runnable(&this).is_ok());
        assert!(!runnable(b"#!/bin/sh\n", 0o755));
        // A universal file, which no release is.
        assert!(!runnable(&[0xca, 0xfe, 0xba, 0xbe, 0, 0, 0, 0x01], 0o755));
    }

    /// The program inside an app bundle, named as the running program is.
    fn program(bundle: &Path) -> PathBuf {
        let executable = std::env::current_exe().unwrap();
        bundle
            .join("Contents/MacOS")
            .join(executable.file_name().unwrap())
    }

    /// A bundle whose program is an arm64 header followed by `tag`.
    fn make_bundle(bundle: &Path, tag: &str) {
        write_program(
            &program(bundle),
            &[&THIN_ARM64, tag.as_bytes()].concat(),
            0o755,
        );
    }

    fn tag(bundle: &Path) -> String {
        let bytes = fs::read(program(bundle)).unwrap();
        String::from_utf8_lossy(&bytes[THIN_ARM64.len()..]).into_owned()
    }

    /// A bundle whose program is a real arm64 program with `body` as its
    /// `main`. The compiler driver that links this test program builds it.
    fn build_bundle(bundle: &Path, body: &str) {
        let program = program(bundle);
        fs::create_dir_all(program.parent().unwrap()).unwrap();
        let source = bundle.with_extension("c");
        fs::write(
            &source,
            format!("#include <string.h>\nint main(int argc, char **argv) {{ {body} }}\n"),
        )
        .unwrap();
        let built = Command::new("cc")
            .arg("-o")
            .arg(&program)
            .arg(&source)
            .status()
            .unwrap();
        assert!(built.success());
        fs::remove_file(source).unwrap();
    }

    /// An installed bundle tagged "old" and a staged package whose bundle is
    /// made by `fill`. Returns the installed bundle and the package.
    fn setup(root: &Path, fill: impl FnOnce(&Path)) -> (PathBuf, PathBuf) {
        let installed = root.join("Applications/IP-KVM OS Services.app");
        make_bundle(&installed, "old");
        let source = root.join("source");
        fill(&source.join("IP-KVM OS Services.app"));
        let archive = platform::staging_path(&installed);
        fs::create_dir_all(archive.parent().unwrap()).unwrap();
        // Every bundle that `fill` made goes into the package.
        let packed = Command::new("tar")
            .arg("-czf")
            .arg(&archive)
            .arg("-C")
            .arg(&source)
            .arg(".")
            .status()
            .unwrap();
        assert!(packed.success());
        (installed, archive)
    }

    /// The new program must not replace this process: TCC would check it as
    /// the previous bundle. The install starts it one time, with the version
    /// flag only, and the program here ends with an error for any other
    /// arguments.
    #[test]
    fn install_swaps_the_bundle_and_leaves_the_restart_to_launchd() {
        let root = Root::new("macos-update");
        let (installed, archive) = setup(&root.0, |bundle| {
            build_bundle(
                bundle,
                r#"return !(argc == 2 && strcmp(argv[1], "--version") == 0);"#,
            );
        });
        let new = fs::read(program(&root.0.join("source/IP-KVM OS Services.app"))).unwrap();

        platform::install(&installed, &archive).unwrap();

        assert!(fs::read(program(&installed)).unwrap() == new);
        assert!(!archive.parent().unwrap().exists());
    }

    #[test]
    fn install_refuses_a_program_that_does_not_start() {
        let root = Root::new("macos-update-no-start");
        let (installed, archive) = setup(&root.0, |bundle| build_bundle(bundle, "return 1;"));

        let refused = platform::install(&installed, &archive).unwrap_err();

        assert!(refused.to_string().contains("does not start"));
        assert_eq!(tag(&installed), "old");
        assert!(!archive.with_file_name("unpacked").exists());
    }

    /// A bundle around a copy of this test program. The linker signs every
    /// arm64 program ad hoc, so the bundle is signed as a development build
    /// is: with no certificate and no team.
    fn make_development_bundle(bundle: &Path) {
        let executable = std::env::current_exe().unwrap();
        let name = executable.file_name().unwrap().to_str().unwrap();
        fs::create_dir_all(bundle.join("Contents/MacOS")).unwrap();
        fs::copy(&executable, program(bundle)).unwrap();
        fs::write(
            bundle.join("Contents/Info.plist"),
            format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict>
<key>CFBundleExecutable</key><string>{name}</string>
<key>CFBundleIdentifier</key><string>com.example.development</string>
<key>CFBundlePackageType</key><string>APPL</string>
</dict></plist>
"#
            ),
        )
        .unwrap();
    }

    #[test]
    fn an_installed_app_without_a_team_accepts_every_package() {
        let root = Root::new("macos-publisher-skipped");
        let unsigned = root.0.join("unsigned.app");
        make_bundle(&unsigned, "old");
        let development = root.0.join("development.app");
        make_development_bundle(&development);
        let package = root.0.join("package.app.tar.gz");
        fs::write(&package, b"not an archive").unwrap();

        assert_eq!(platform::team(&unsigned), None);
        assert_eq!(platform::team(&development), None);
        assert_eq!(platform::check_publisher(&unsigned, &package), Ok(()));
        assert_eq!(platform::check_publisher(&development, &package), Ok(()));
    }

    #[test]
    fn an_app_without_the_signature_of_the_team_is_refused() {
        let root = Root::new("macos-publisher-refused");
        let unsigned = root.0.join("unsigned.app");
        make_bundle(&unsigned, "new");
        let development = root.0.join("development.app");
        make_development_bundle(&development);

        assert!(platform::require_team(&unsigned, "ABCDE12345").is_err());
        assert!(platform::require_team(&development, "ABCDE12345").is_err());
        assert!(platform::require_team(&root.0.join("missing.app"), "ABCDE12345").is_err());
    }

    #[test]
    fn install_refuses_a_program_file_that_is_not_arm64() {
        let root = Root::new("macos-update-refused");
        let (installed, archive) = setup(&root.0, |bundle| {
            write_program(&program(bundle), b"#!/bin/sh\n", 0o755);
        });

        assert!(platform::install(&installed, &archive).is_err());

        assert_eq!(tag(&installed), "old");
        assert!(!archive.with_file_name("unpacked").exists());
    }

    #[test]
    fn install_refuses_a_package_with_two_bundles() {
        let root = Root::new("macos-update-two-bundles");
        let (installed, archive) = setup(&root.0, |bundle| {
            make_bundle(bundle, "new");
            make_bundle(&bundle.with_file_name("Second.app"), "second");
        });

        assert!(platform::install(&installed, &archive).is_err());

        assert_eq!(tag(&installed), "old");
        assert!(!archive.with_file_name("unpacked").exists());
    }
}
