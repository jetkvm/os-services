//! The two root-only installs Linux needs. The udev rule lets desktop users
//! open the KVM's USB device. The capture helper is the one binary with
//! CAP_SYS_ADMIN: the sender loads libdrmtap in-process, and libdrmtap asks
//! the helper for scanout buffers. Execution of the helper is limited to the
//! ipkvm-capture group.

use super::run;
use std::{
    error::Error,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

pub const USB_ACCESS_FLAG: &str = "--install-linux-usb-access";
pub const CAPTURE_ACCESS_FLAG: &str = "--install-linux-capture-access";
/// Both installs, and capture for the user who asked; the app runs this
/// through pkexec when the KVM cannot be opened.
pub const ACCESS_FLAG: &str = "--install-linux-access";

const USB_RULE: &str = include_str!("../../packaging/linux/70-ipkvm-os-services.rules");
const USB_RULE_PATH: &str = "/etc/udev/rules.d/70-ipkvm-os-services.rules";
/// Where udev reads rules, first to last. A file in an earlier folder hides
/// the file with the same name in the later ones.
const UDEV_RULE_DIRS: [&str; 5] = [
    "/etc/udev/rules.d",
    "/run/udev/rules.d",
    "/usr/local/lib/udev/rules.d",
    "/usr/lib/udev/rules.d",
    "/lib/udev/rules.d",
];
/// Where the kernel lists every USB device and interface.
const USB_DEVICES: &str = "/sys/bus/usb/devices";
/// The class, subclass and protocol of the OS Services interface, which the
/// rule matches (PROTOCOL.md).
const OS_SERVICES_INTERFACE: [u8; 3] = [0xff, 0x4a, 0x01];
/// Where the helper goes to hold its capability: the files of the service
/// belong to the user.
const INSTALLED_HELPER: &str = "/usr/local/libexec/drmtap-helper";
/// The file next to the installed helper that states its version.
const INSTALLED_HELPER_VERSION: &str = "/usr/local/libexec/drmtap-helper.version";
/// The version of the capture helper that this build carries. An update
/// from the KVM replaces the service and leaves the helper, which only an
/// administrator can replace, so the service compares versions: it captures
/// with a helper of this version or a newer one. Raise the number with each
/// change to the helper that `packaging/libdrmtap/build.sh` builds: its
/// commit, a patch to `helper/`, or a build option.
const HELPER_VERSION: u32 = 1;
/// A file capability applies to everyone who can execute the file, so the
/// mode and group are what keep other local accounts from reading the screen.
const CAPTURE_GROUP: &str = "ipkvm-capture";
/// The library directory of the AppImage's files, relative to the program.
const BUNDLE_DIR: &str = "../lib/ipkvm-os-services";

pub fn install_usb_access() -> Result<(), Box<dyn Error>> {
    require_root()?;
    std::fs::write(USB_RULE_PATH, USB_RULE)
        .map_err(|error| format!("could not write {USB_RULE_PATH}: {error}"))?;
    std::fs::set_permissions(USB_RULE_PATH, std::fs::Permissions::from_mode(0o644))
        .map_err(|error| format!("could not set permissions on {USB_RULE_PATH}: {error}"))?;
    run("udevadm", &["control", "--reload-rules"])?;
    // A KVM attached now gets access without being plugged in again. The
    // trigger names the device alone: a device path would also trigger its
    // interfaces and every device under them.
    for kvm in kvm_devices(Path::new(USB_DEVICES)) {
        run(
            "udevadm",
            &[
                "trigger",
                "--action=change",
                "--subsystem-match=usb",
                &format!("--sysname-match={kvm}"),
            ],
        )?;
    }
    Ok(())
}

/// Whether udev uses this build's USB access rule. A rule with other content
/// does not count, nor does this rule behind another with the same name.
pub fn usb_rule_installed() -> bool {
    rule_in(UDEV_RULE_DIRS.map(Path::new))
}

fn rule_in(dirs: impl IntoIterator<Item = impl AsRef<Path>>) -> bool {
    let name = Path::new(USB_RULE_PATH)
        .file_name()
        .expect("rule path names a file");
    dirs.into_iter()
        .find_map(|dir| std::fs::read_to_string(dir.as_ref().join(name)).ok())
        .is_some_and(|rule| rule == USB_RULE)
}

/// The USB access rule and the capture helper, with capture for the user who
/// ran this through pkexec or sudo. That user asked for it from the desktop
/// and gave an administrator's password, so the group is theirs to join.
pub fn install_access() -> Result<(), Box<dyn Error>> {
    install_usb_access()?;
    install_capture_helper()?;
    if let Some(user) = invoking_user() {
        run("usermod", &["-aG", CAPTURE_GROUP, &user])?;
        println!("added {user} to {CAPTURE_GROUP}; capture starts after {user} logs in again");
    }
    Ok(())
}

/// The account that ran pkexec or sudo, when it is not root.
fn invoking_user() -> Option<String> {
    let uid: libc::uid_t = ["PKEXEC_UID", "SUDO_UID"]
        .iter()
        .find_map(|name| std::env::var(name).ok()?.parse().ok())?;
    if uid == 0 {
        return None;
    }
    // SAFETY: the process is single-threaded while it runs an install, and
    // the entry is copied before the next call.
    let entry = unsafe { libc::getpwuid(uid) };
    if entry.is_null() {
        return None;
    }
    let name = unsafe { std::ffi::CStr::from_ptr((*entry).pw_name) };
    name.to_str().ok().map(str::to_owned)
}

/// The names of the attached USB devices that offer the OS Services
/// interface, such as `1-2`. `devices` lists devices and interfaces alike;
/// an interface's directory is inside its device's.
fn kvm_devices(devices: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(devices) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(|entry| Some(entry.ok()?.path()))
        .filter(|interface| is_os_services(interface))
        .filter_map(|interface| {
            let interface = std::fs::canonicalize(interface).ok()?;
            Some(interface.parent()?.file_name()?.to_str()?.to_owned())
        })
        .collect();
    names.sort();
    names.dedup();
    names
}

/// Whether a sysfs entry is an interface with the OS Services class,
/// subclass and protocol. A device has none of these attributes.
fn is_os_services(interface: &Path) -> bool {
    let byte = |name: &str| hex_byte(&std::fs::read_to_string(interface.join(name)).ok()?);
    [
        byte("bInterfaceClass"),
        byte("bInterfaceSubClass"),
        byte("bInterfaceProtocol"),
    ] == OS_SERVICES_INTERFACE.map(Some)
}

/// One byte as sysfs shows it: two hex digits and a newline, such as "4a\n".
fn hex_byte(text: &str) -> Option<u8> {
    u8::from_str_radix(text.trim(), 16).ok()
}

/// Installs the capture helper. Adding users to the group is left to the
/// administrator.
pub fn install_capture_access() -> Result<(), Box<dyn Error>> {
    install_capture_helper()?;
    println!("add each user that may capture: usermod -aG {CAPTURE_GROUP} <user>");
    Ok(())
}

/// Copies the AppImage's helper to /usr/local and gives it the capability.
fn install_capture_helper() -> Result<(), Box<dyn Error>> {
    require_root()?;
    // All users of the computer share the helper. A user with an older
    // service keeps the newer helper of another user's service.
    if newer_helper_installed(Path::new(INSTALLED_HELPER_VERSION)) {
        println!("the installed capture helper is newer than this version's; it stays");
        return Ok(());
    }
    let source = bundled("drmtap-helper").ok_or("this build does not carry drmtap-helper")?;
    let target = Path::new(INSTALLED_HELPER);
    let directory = target.parent().expect("helper path has a parent");
    std::fs::create_dir_all(directory)
        .map_err(|error| format!("could not create {}: {error}", directory.display()))?;
    // Replace rather than overwrite: writing into a running binary that
    // carries a capability fails with ETXTBSY.
    let staged = target.with_extension("new");
    std::fs::copy(&source, &staged)
        .map_err(|error| format!("could not copy the helper: {error}"))?;
    if let Err(error) = grant(&staged) {
        let _ = std::fs::remove_file(&staged);
        return Err(error);
    }
    std::fs::rename(&staged, target)
        .map_err(|error| format!("could not install the helper: {error}"))?;
    let version = Path::new(INSTALLED_HELPER_VERSION);
    std::fs::write(version, format!("{HELPER_VERSION}\n"))
        .and_then(|()| std::fs::set_permissions(version, std::fs::Permissions::from_mode(0o644)))
        .map_err(|error| format!("could not write {INSTALLED_HELPER_VERSION}: {error}").into())
}

/// How the installed capture helper compares with the one this build
/// carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Helper {
    /// The installed helper has this build's version or a newer one, or
    /// there is nothing to compare: no helper is installed, this build
    /// carries none, or this user may not read the installed one.
    Usable,
    /// The installed helper is older than this build's. The service does
    /// not capture with it.
    Older,
}

/// The state of the installed capture helper.
pub fn helper() -> Helper {
    match bundled("drmtap-helper") {
        Some(bundled) => helper_state(
            &bundled,
            Path::new(INSTALLED_HELPER),
            Path::new(INSTALLED_HELPER_VERSION),
        ),
        None => Helper::Usable,
    }
}

fn helper_state(bundled: &Path, installed: &Path, version: &Path) -> Helper {
    if let Ok(version) = std::fs::read_to_string(version) {
        let current = version
            .trim()
            .parse()
            .is_ok_and(|version: u32| version >= HELPER_VERSION);
        return if current {
            Helper::Usable
        } else {
            Helper::Older
        };
    }
    // An install without a version file is older than every version, unless
    // its helper is this build's file.
    match (std::fs::read(bundled), std::fs::read(installed)) {
        (Ok(bundled), Ok(installed)) if bundled != installed => Helper::Older,
        _ => Helper::Usable,
    }
}

/// Whether the installed helper states a newer version than this build's.
fn newer_helper_installed(version: &Path) -> bool {
    std::fs::read_to_string(version)
        .ok()
        .and_then(|version| version.trim().parse().ok())
        .is_some_and(|version: u32| version > HELPER_VERSION)
}

/// Points the capture stack at the library and helper this build carries.
/// Called once at startup, before any thread reads the environment.
pub fn announce_bundled_paths() {
    if std::env::var_os("OS_SERVICES_DRMTAP_LIB").is_none() {
        if let Some(library) = bundled("libdrmtap.so.0") {
            // SAFETY: no other thread exists yet.
            unsafe { std::env::set_var("OS_SERVICES_DRMTAP_LIB", library) };
        }
    }
    if std::env::var_os("OS_SERVICES_DRMTAP_HELPER").is_none() {
        // The installed helper holds the capability; the bundled copy does
        // not.
        // SAFETY: as above.
        unsafe { std::env::set_var("OS_SERVICES_DRMTAP_HELPER", INSTALLED_HELPER) };
    }
}

fn require_root() -> Result<(), &'static str> {
    if unsafe { libc::geteuid() } != 0 {
        return Err("administrator privileges are required");
    }
    Ok(())
}

/// A file this build carries in its library directory. The files of an
/// AppImage hold the helper and the library; a plain cargo build has
/// neither.
fn bundled(name: &str) -> Option<PathBuf> {
    let executable = std::env::current_exe().ok()?;
    let path = executable.parent()?.join(BUNDLE_DIR).join(name);
    path.exists().then_some(path)
}

/// Gives the helper its capability, and the group that may run it.
fn grant(helper: &Path) -> Result<(), Box<dyn Error>> {
    std::fs::set_permissions(helper, std::fs::Permissions::from_mode(0o750))
        .map_err(|error| format!("could not set permissions on the helper: {error}"))?;
    let path = helper.to_string_lossy();
    run("groupadd", &["-f", CAPTURE_GROUP])?;
    run("chown", &[&format!("root:{CAPTURE_GROUP}"), &path])?;
    run("setcap", &["cap_sys_admin+ep", &path]).map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, os::unix::fs::symlink};

    /// A sysfs tree in a temporary folder, laid out as the kernel does:
    /// devices and interfaces nest under `devices/`, and the bus links to
    /// each of them by name.
    struct Sysfs {
        root: PathBuf,
    }

    impl Sysfs {
        fn new(name: &str) -> Self {
            let root = std::env::temp_dir().join(format!("sysfs-{}-{name}", std::process::id()));
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(root.join("bus/usb/devices")).unwrap();
            Self { root }
        }

        fn bus(&self) -> PathBuf {
            self.root.join("bus/usb/devices")
        }

        /// Adds a device at `path` under `devices/`.
        fn device(&self, path: &str) {
            fs::write(self.add(path).join("bDeviceClass"), "00\n").unwrap();
        }

        /// Adds an interface at `path` under `devices/`, inside its device.
        fn interface(&self, path: &str, [class, subclass, protocol]: [&str; 3]) {
            let interface = self.add(path);
            for (name, value) in [
                ("bInterfaceClass", class),
                ("bInterfaceSubClass", subclass),
                ("bInterfaceProtocol", protocol),
            ] {
                fs::write(interface.join(name), format!("{value}\n")).unwrap();
            }
        }

        fn add(&self, path: &str) -> PathBuf {
            let entry = self.root.join("devices").join(path);
            fs::create_dir_all(&entry).unwrap();
            let name = entry.file_name().unwrap();
            symlink(
                Path::new("../../../devices").join(path),
                self.bus().join(name),
            )
            .unwrap();
            entry
        }
    }

    #[test]
    fn finds_the_devices_with_the_os_services_interface() {
        let sysfs = Sysfs::new("kvms");
        let hub = "pci0000:00/0000:00:14.0/usb1";
        sysfs.device(hub);
        sysfs.interface(&format!("{hub}/1-0:1.0"), ["09", "00", "00"]);
        // A KVM with a keyboard and two OS Services interfaces.
        sysfs.device(&format!("{hub}/1-2"));
        sysfs.interface(&format!("{hub}/1-2/1-2:1.0"), ["03", "01", "01"]);
        sysfs.interface(&format!("{hub}/1-2/1-2:1.5"), ["ff", "4a", "01"]);
        sysfs.interface(&format!("{hub}/1-2/1-2:1.6"), ["ff", "4a", "01"]);
        // Vendor interfaces that are not OS Services.
        sysfs.device(&format!("{hub}/1-3"));
        sysfs.interface(&format!("{hub}/1-3/1-3:1.0"), ["ff", "4a", "02"]);
        sysfs.interface(&format!("{hub}/1-3/1-3:1.1"), ["ff", "00", "00"]);
        // A KVM behind an external hub.
        sysfs.device(&format!("{hub}/1-4"));
        sysfs.interface(&format!("{hub}/1-4/1-4:1.0"), ["09", "00", "00"]);
        sysfs.device(&format!("{hub}/1-4/1-4.1"));
        sysfs.interface(&format!("{hub}/1-4/1-4.1/1-4.1:1.0"), ["FF", "4A", "01"]);
        assert_eq!(kvm_devices(&sysfs.bus()), ["1-2", "1-4.1"]);
    }

    #[test]
    fn no_usb_bus_has_no_kvm() {
        let sysfs = Sysfs::new("empty");
        assert!(kvm_devices(&sysfs.bus()).is_empty());
        assert!(kvm_devices(&sysfs.root.join("missing")).is_empty());
    }

    #[test]
    fn reads_a_byte_as_sysfs_shows_it() {
        assert_eq!(hex_byte("ff\n"), Some(0xff));
        assert_eq!(hex_byte("4a\n"), Some(0x4a));
        assert_eq!(hex_byte("01"), Some(0x01));
        assert_eq!(hex_byte("4A\n"), Some(0x4a));
        assert_eq!(hex_byte(""), None);
        assert_eq!(hex_byte("100\n"), None);
        assert_eq!(hex_byte("zz\n"), None);
    }

    #[test]
    fn only_this_builds_rule_in_effect_counts_as_installed() {
        let root = std::env::temp_dir().join(format!("rules-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let (admin, package) = (root.join("etc"), root.join("usr-lib"));
        fs::create_dir_all(&admin).unwrap();
        fs::create_dir_all(&package).unwrap();
        let rule = |dir: &Path| dir.join("70-ipkvm-os-services.rules");
        assert!(!rule_in([&admin, &package]));
        // A rule with other content.
        fs::write(
            rule(&admin),
            "SUBSYSTEM==\"usb\", ATTR{idVendor}==\"1d6b\"\n",
        )
        .unwrap();
        assert!(!rule_in([&admin, &package]));
        // udev reads the other rule in /etc instead of this one.
        fs::write(rule(&package), USB_RULE).unwrap();
        assert!(!rule_in([&admin, &package]));
        assert!(rule_in([root.join("missing"), package.clone()]));
        fs::write(rule(&admin), USB_RULE).unwrap();
        assert!(rule_in([&admin, &package]));
    }

    #[test]
    fn only_a_helper_older_than_this_builds_is_not_usable() {
        let root = std::env::temp_dir().join(format!("helper-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let (bundled, installed) = (root.join("bundled"), root.join("installed"));
        let version = root.join("version");
        let state = || helper_state(&bundled, &installed, &version);
        fs::write(&bundled, b"this build's helper").unwrap();
        // No helper is installed, so capture was not set up.
        assert_eq!(state(), Helper::Usable);

        // A helper that a release without version files installed.
        fs::write(&installed, b"another helper").unwrap();
        assert_eq!(state(), Helper::Older);
        fs::copy(&bundled, &installed).unwrap();
        assert_eq!(state(), Helper::Usable);

        // The version decides, whatever the file holds.
        fs::write(&installed, b"another helper").unwrap();
        for (stated, expected) in [
            (HELPER_VERSION - 1, Helper::Older),
            (HELPER_VERSION, Helper::Usable),
            (HELPER_VERSION + 1, Helper::Usable),
        ] {
            fs::write(&version, format!("{stated}\n")).unwrap();
            assert_eq!(state(), expected, "version {stated}");
            assert_eq!(newer_helper_installed(&version), stated > HELPER_VERSION);
        }
        fs::write(&version, "not a number").unwrap();
        assert_eq!(state(), Helper::Older);
        assert!(!newer_helper_installed(&version));
        fs::remove_dir_all(&root).unwrap();
    }

    /// SHA-256 of the source that `HELPER_VERSION` stands for: the commit
    /// line of `packaging/libdrmtap/build.sh` and each patch there that
    /// changes a file under `helper/`.
    const HELPER_SOURCE: &str = "a286f75b6238549047a5a5185cad089259f0131c2b6df3e5b6ad1360e43394d4";

    /// An installed helper is replaced only when the version rises, so a
    /// change to the helper's source without a new version would stay on the
    /// computers that have the old helper.
    #[test]
    fn the_helper_version_rises_with_the_helper_source() {
        use sha2::{Digest, Sha256};
        let folder = Path::new(env!("CARGO_MANIFEST_DIR")).join("packaging/libdrmtap");
        let build = fs::read_to_string(folder.join("build.sh")).unwrap();
        let commit = build.lines().find(|line| line.starts_with("COMMIT="));
        let mut source = Sha256::new();
        source.update(commit.expect("build.sh states the commit"));
        let mut patches: Vec<PathBuf> = fs::read_dir(&folder)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "patch")
            })
            .collect();
        patches.sort();
        for patch in patches {
            let patch = fs::read_to_string(patch).unwrap();
            if patch.contains(" a/helper/") {
                source.update(patch);
            }
        }
        let digest: String = source
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        assert_eq!(
            digest, HELPER_SOURCE,
            "the source of the capture helper changed: raise HELPER_VERSION, \
             and state this digest in HELPER_SOURCE"
        );
    }

    #[test]
    fn the_rule_matches_the_os_services_interface() {
        let [class, subclass, protocol] = OS_SERVICES_INTERFACE;
        let interfaces = format!("*:{class:02x}{subclass:02x}{protocol:02x}:*");
        let rule = format!("ENV{{ID_USB_INTERFACES}}==\"{interfaces}\", TAG+=\"uaccess\"");
        assert!(USB_RULE.lines().any(|line| line.ends_with(&rule)));
    }
}
