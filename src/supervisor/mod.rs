//! How the sender runs under the operating system. The sender runs in the
//! desktop user's session, started by the OS: a systemd user unit on Linux,
//! a launchd agent on macOS, and a scheduled task on Windows. The OS owns
//! startup, restart and logs; the KVM owns every streaming and access
//! setting. A Linux computer without systemd has no such service manager,
//! and `keeper` does those jobs there.
//!
//! The OS starts the sender with `--run`. Without arguments, the sender is
//! the app a user opens from a file manager, shortcut or menu: it registers
//! and starts the service, tells the user, and exits.
//!
//! A sender that is asked to stop ends its session with session.close, so
//! the KVM drops the session at once. On Linux and macOS the service manager
//! asks with a signal. Windows has no signal for a program without a
//! console; `windows` holds what replaces it, and `--stop` asks from the
//! command line.

#[cfg(all(windows, target_arch = "x86_64"))]
mod authenticode;
#[cfg(target_os = "linux")]
mod installed;
#[cfg(target_os = "linux")]
pub mod keeper;
#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(target_os = "linux")]
mod publisher;
mod unit_files;
pub mod update;
#[cfg(windows)]
pub mod windows;

use crate::{approval::PinStore, protocol::Fingerprint};
use std::{error::Error, ffi::OsString};

#[cfg(not(windows))]
const USAGE: &str = "usage: ipkvm-os-services [--run | --install-service | --uninstall-service | \
                     --approve-kvm <fingerprint> | --forget-kvm <fingerprint> | --version]";
#[cfg(windows)]
const USAGE: &str = "usage: ipkvm-os-services [--run | --stop | --install-service | \
                     --uninstall-service | --approve-kvm <fingerprint> | \
                     --forget-kvm <fingerprint> | --version]";

/// Prints the version. An update on Linux and on macOS runs the new program
/// with this flag to see that it starts (see `update::start`).
pub const VERSION_FLAG: &str = "--version";

/// What the user sees after opening the app.
const OPENED: &str = "IP-KVM OS Services is running, and starts each time you sign in.";

/// What this invocation does.
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    /// Register and start the service, and show the user the result.
    Open,
    /// Serve the KVM until stopped. The registered service runs this.
    Run,
    /// Ask this user's running sender to end its session and exit, and wait
    /// until it has. The registration stays, so the service starts again at
    /// the next sign-in.
    #[cfg(windows)]
    Stop,
    /// Register and start the service without a message; for installers.
    InstallService,
    UninstallService,
    /// Print the version.
    Version,
    /// Add a KVM to the user's approved list.
    ApproveKvm(Fingerprint),
    /// Remove a KVM from the user's approved list.
    ForgetKvm(Fingerprint),
    /// Show the approval dialog with this text; the service runs it as a
    /// separate process.
    #[cfg(windows)]
    ApprovalDialog(String),
    /// Install the udev rule that lets desktop users open the KVM. Needs
    /// root.
    #[cfg(target_os = "linux")]
    InstallUsbAccess,
    /// Install the capture helper and its group. Needs root.
    #[cfg(target_os = "linux")]
    InstallCaptureAccess,
    /// Both installs, and the capture group for the user who ran pkexec or
    /// sudo. Needs root.
    #[cfg(target_os = "linux")]
    InstallAccess,
}

impl Action {
    /// The action for the arguments after the program name.
    pub fn from_args(args: &[OsString]) -> Result<Self, String> {
        let args: Vec<&str> = args
            .iter()
            .map(|arg| arg.to_str().unwrap_or_default())
            .collect();
        match args[..] {
            [] => Ok(Self::Open),
            ["--run"] => Ok(Self::Run),
            [VERSION_FLAG] => Ok(Self::Version),
            #[cfg(windows)]
            ["--stop"] => Ok(Self::Stop),
            ["--approve-kvm", kvm] => Ok(Self::ApproveKvm(Fingerprint::parse(kvm)?)),
            ["--forget-kvm", kvm] => Ok(Self::ForgetKvm(Fingerprint::parse(kvm)?)),
            #[cfg(windows)]
            ["--approval-dialog", text] => Ok(Self::ApprovalDialog(text.into())),
            ["--install-service"] => Ok(Self::InstallService),
            ["--uninstall-service"] => Ok(Self::UninstallService),
            #[cfg(target_os = "linux")]
            [linux::USB_ACCESS_FLAG] => Ok(Self::InstallUsbAccess),
            #[cfg(target_os = "linux")]
            [linux::CAPTURE_ACCESS_FLAG] => Ok(Self::InstallCaptureAccess),
            #[cfg(target_os = "linux")]
            [linux::ACCESS_FLAG] => Ok(Self::InstallAccess),
            _ => Err(USAGE.into()),
        }
    }

    /// Performs an administrative action; returns the process exit code.
    pub fn perform(self) -> i32 {
        let result: Result<(), Box<dyn Error>> = match self {
            Self::Open => {
                let result = install_service(true);
                match &result {
                    Ok(()) => platform::tell_user(OPENED, false),
                    Err(error) => platform::tell_user(
                        &format!("IP-KVM OS Services could not start: {error}."),
                        true,
                    ),
                }
                result
            }
            Self::Run => Ok(()),
            Self::Version => {
                println!("{}", crate::protocol::SENDER_VERSION);
                Ok(())
            }
            #[cfg(windows)]
            Self::Stop => windows::stop_sender(),
            Self::InstallService => install_service(true),
            Self::UninstallService => install_service(false),
            Self::ApproveKvm(kvm) => approve_kvm(&kvm),
            Self::ForgetKvm(kvm) => forget_kvm(&kvm),
            #[cfg(windows)]
            Self::ApprovalDialog(text) => {
                return if crate::approval::message_box(&text) {
                    0
                } else {
                    1
                };
            }
            #[cfg(target_os = "linux")]
            Self::InstallUsbAccess => linux::install_usb_access(),
            #[cfg(target_os = "linux")]
            Self::InstallCaptureAccess => linux::install_capture_access(),
            #[cfg(target_os = "linux")]
            Self::InstallAccess => linux::install_access(),
        };
        match result {
            Ok(()) => 0,
            Err(error) => {
                eprintln!("ipkvm-os-services: {error}");
                1
            }
        }
    }
}

/// Opening the app while the service runs reaches the running service on
/// macOS instead of starting the program. The service shows the message and
/// starts the program on disk with `--install-service`, which may be a newer
/// version than this process. That registers the agent again and restarts
/// the service; it runs in its own process group, so it outlives this
/// process when launchd stops the agent.
#[cfg(target_os = "macos")]
pub fn reopened() {
    use std::os::unix::process::CommandExt;
    platform::tell_user(OPENED, false);
    let started = std::env::current_exe().and_then(|program| {
        std::process::Command::new(program)
            .arg("--install-service")
            .process_group(0)
            .spawn()
    });
    if let Err(error) = started {
        eprintln!("ipkvm-os-services: could not register the service again: {error}");
    }
}

/// See `platform::leave_appimage`.
#[cfg(target_os = "linux")]
pub fn leave_appimage() {
    platform::leave_appimage();
}

fn install_service(enable: bool) -> Result<(), Box<dyn Error>> {
    refuse_root("install the service")?;
    platform::install(enable)
}

fn approve_kvm(kvm: &Fingerprint) -> Result<(), Box<dyn Error>> {
    refuse_root("approve a KVM")?;
    PinStore::for_user()?.approve(kvm)?;
    println!("approved the KVM {kvm}");
    Ok(())
}

fn forget_kvm(kvm: &Fingerprint) -> Result<(), Box<dyn Error>> {
    refuse_root("forget a KVM")?;
    if !PinStore::for_user()?.forget(kvm)? {
        return Err(format!("the KVM {kvm} is not approved").into());
    }
    println!("forgot the KVM {kvm}");
    Ok(())
}

/// The service reads the settings of the desktop user, so a command run
/// with sudo would change root's settings instead.
#[cfg_attr(windows, allow(unused_variables))]
fn refuse_root(action: &str) -> Result<(), Box<dyn Error>> {
    #[cfg(unix)]
    if unsafe { libc::geteuid() } == 0 {
        return Err(format!("{action} as the desktop user, without sudo").into());
    }
    Ok(())
}

/// Runs a program and fails unless it exits successfully.
#[cfg(unix)]
fn run(program: &str, args: &[&str]) -> Result<(), String> {
    let status = std::process::Command::new(program)
        .args(args)
        .status()
        .map_err(|error| format!("could not run {program}: {error}"))?;
    if !status.success() {
        return Err(format!("{program} exited with {status}"));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
mod platform {
    use super::{installed, keeper, run, unit_files, Error};
    use std::{
        fs,
        path::{Path, PathBuf},
    };

    const UNIT: &str = unit_files::SYSTEMD_UNIT;

    /// The files that register the service for this user.
    struct Registration {
        unit: PathBuf,
        autostart: PathBuf,
        /// The entry in the applications menu.
        menu: PathBuf,
    }

    impl Registration {
        fn of_user() -> Result<Self, Box<dyn Error>> {
            let config = dirs::config_dir().ok_or("no user configuration directory")?;
            let data = dirs::data_dir().ok_or("no user data directory")?;
            Ok(Self {
                unit: config.join("systemd/user").join(UNIT),
                autostart: config.join("autostart").join(unit_files::AUTOSTART_FILE),
                menu: data.join("applications").join(unit_files::MENU_FILE),
            })
        }
    }

    /// Registers the service for this user and starts it, or removes the
    /// registration.
    pub fn install(enable: bool) -> Result<(), Box<dyn Error>> {
        let registration = Registration::of_user()?;
        if enable {
            let program = register(&registration)?;
            return start(&program);
        }
        for entry in [&registration.autostart, &registration.menu] {
            if entry.exists() {
                fs::remove_file(entry)?;
            }
        }
        if registration.unit.exists() {
            if keeper::systemd() {
                remove_user_unit(&registration.unit)?;
            } else {
                fs::remove_file(&registration.unit)?;
            }
        }
        keeper::stop()
    }

    /// Installs the files of an AppImage in the user's folder (see
    /// `installed`), and registers the program there with a unit, an
    /// autostart entry and an entry in the applications menu. A plain cargo
    /// build has no such files: the unit starts the build where it is, and
    /// the menu gets no entry. A computer without systemd gets no unit (see
    /// `keeper`). Returns the registered program.
    fn register(registration: &Registration) -> Result<PathBuf, Box<dyn Error>> {
        let folder = installed::install()?;
        let program = match &folder {
            Some(folder) => installed::program(folder),
            None => std::env::current_exe()?,
        };
        if keeper::systemd() {
            point_unit(&registration.unit, &program)?;
        }
        complete(registration, folder.as_deref(), &program)?;
        Ok(program)
    }

    /// Makes the unit start the program. If systemd does not take the new
    /// unit, the earlier unit file comes back, so the file and systemd
    /// agree about what the unit starts.
    fn point_unit(unit: &Path, program: &Path) -> Result<(), Box<dyn Error>> {
        let program = program.to_str().ok_or("executable path is not UTF-8")?;
        let text = unit_files::systemd_unit(program)?;
        let earlier = fs::read(unit).ok();
        write(unit, &text)?;
        if let Err(error) = run("systemctl", &["--user", "daemon-reload"]) {
            let _ = match earlier {
                Some(earlier) => fs::write(unit, earlier),
                None => fs::remove_file(unit),
            };
            return Err(error.into());
        }
        Ok(())
    }

    /// The parts of the registration around the unit: its start with the
    /// graphical session, the autostart entry, and the menu entry of a
    /// program in the user's folder. Without systemd, the autostart entry
    /// starts the program itself.
    fn complete(
        registration: &Registration,
        folder: Option<&Path>,
        program: &Path,
    ) -> Result<(), Box<dyn Error>> {
        let program = program.to_str().ok_or("executable path is not UTF-8")?;
        let autostart = if keeper::systemd() {
            run("systemctl", &["--user", "enable", UNIT])?;
            unit_files::autostart_entry()
        } else {
            unit_files::autostart_entry_without_systemd(program)?
        };
        write(&registration.autostart, &autostart)?;
        if let Some(folder) = folder {
            let icon = installed::icon(folder);
            let icon = icon.to_str().ok_or("icon path is not UTF-8")?;
            write(&registration.menu, &unit_files::menu_entry(program, icon)?)?;
        }
        Ok(())
    }

    fn write(file: &Path, text: &str) -> Result<(), Box<dyn Error>> {
        fs::create_dir_all(file.parent().ok_or("a registration file has no folder")?)?;
        Ok(fs::write(file, text)?)
    }

    fn remove_user_unit(unit: &Path) -> Result<(), Box<dyn Error>> {
        run("systemctl", &["--user", "disable", "--now", UNIT])?;
        fs::remove_file(unit)?;
        run("systemctl", &["--user", "daemon-reload"]).map_err(Into::into)
    }

    /// A unit that starts an AppImage belongs to an installation that runs
    /// from the image. When that unit started this program, the program
    /// moves to the user's folder: it installs its files there, points the
    /// unit at them, asks systemd to start the unit again, and exits.
    ///
    /// Until the unit starts the program in the folder, a failure leaves
    /// the service running from the image, and it can still update. Once
    /// the unit starts the program in the folder, this process always
    /// exits: nothing starts the image again, so an update of the image
    /// would never run.
    pub fn leave_appimage() {
        let Some(image) = std::env::var_os("APPIMAGE") else {
            return;
        };
        let Ok(registration) = Registration::of_user() else {
            return;
        };
        let started_by_unit = fs::read_to_string(&registration.unit)
            .is_ok_and(|unit| unit_files::unit_starts(&unit, &image.to_string_lossy()));
        if !started_by_unit {
            return;
        }
        let moved = installed::install().and_then(|folder| {
            let folder = folder.ok_or("this program has no AppImage files")?;
            let program = installed::program(&folder);
            point_unit(&registration.unit, &program)?;
            Ok((folder, program))
        });
        let (folder, program) = match moved {
            Ok(moved) => moved,
            Err(error) => {
                eprintln!(
                    "ipkvm-os-services: could not move from the AppImage to the user's \
                     folder: {error}"
                );
                return;
            }
        };
        eprintln!("ipkvm-os-services: moved from the AppImage to the user's folder");
        if let Err(error) = complete(&registration, Some(&folder), &program) {
            eprintln!("ipkvm-os-services: the registration is not complete: {error}");
        }
        let restart = run("systemctl", &["--user", "--no-block", "restart", UNIT]);
        // Without the restart, the unit's Restart=on-failure starts the
        // program in the folder after an unsuccessful exit.
        std::process::exit(i32::from(restart.is_err()));
    }

    /// Restarts the service with the session's display variables. Without
    /// systemd, a keeper that this process starts has those variables.
    fn start(program: &Path) -> Result<(), Box<dyn Error>> {
        if !keeper::systemd() {
            return keeper::start(program);
        }
        let mut args = vec!["--user", "import-environment"];
        args.extend(unit_files::SESSION_VARIABLES);
        run("systemctl", &args)?;
        run("systemctl", &["--user", "restart", UNIT]).map_err(Into::into)
    }

    /// Prints the message, or shows it as a desktop notification when a file
    /// manager started the program. `perform` also prints errors to stderr.
    pub fn tell_user(text: &str, error: bool) {
        use std::io::IsTerminal;
        if std::io::stdout().is_terminal() {
            if !error {
                println!("{text}");
            }
            return;
        }
        let urgency = if error { "critical" } else { "normal" };
        let _ = std::process::Command::new("notify-send")
            .args(["--urgency", urgency, "IP-KVM OS Services", text])
            .status();
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use super::{run, unit_files, Error};
    use std::{
        process::Command,
        thread,
        time::{Duration, Instant},
    };

    pub fn install(enable: bool) -> Result<(), Box<dyn Error>> {
        let home = dirs::home_dir().ok_or("no home directory")?;
        let agents = home.join("Library/LaunchAgents");
        let plist = agents.join(format!("{}.plist", unit_files::LAUNCHD_LABEL));
        let domain = format!("gui/{}", unsafe { libc::getuid() });
        let target = format!("{domain}/{}", unit_files::LAUNCHD_LABEL);
        let executable = std::env::current_exe()?;
        let path = executable.to_string_lossy();
        // An app opened from the DMG, or from a download folder that macOS
        // runs from a random path, is gone after the next restart.
        if enable && (path.starts_with("/Volumes/") || path.contains("/AppTranslocation/")) {
            return Err(
                "move IP-KVM OS Services to the Applications folder, then open it from there"
                    .into(),
            );
        }
        unload(&target)?;
        if !enable {
            if plist.exists() {
                std::fs::remove_file(plist)?;
            }
            return Ok(());
        }
        let log = super::log_path()?;
        std::fs::create_dir_all(log.parent().expect("the log is in a folder"))?;
        std::fs::create_dir_all(&agents)?;
        std::fs::write(
            &plist,
            unit_files::launchd_plist(&path, &log.to_string_lossy()),
        )?;
        let plist = plist.to_str().ok_or("invalid agent path")?;
        run("launchctl", &["bootstrap", &domain, plist]).map_err(Into::into)
    }

    pub fn tell_user(text: &str, error: bool) {
        crate::macos::show_message(text, error);
    }

    /// Unloads the agent and waits until launchd has removed it: bootout
    /// returns first, and a new agent cannot load into the domain until the
    /// old one is gone.
    fn unload(target: &str) -> Result<(), Box<dyn Error>> {
        let loaded = || -> std::io::Result<bool> {
            let output = Command::new("launchctl").args(["print", target]).output()?;
            Ok(output.status.success())
        };
        if !loaded()? {
            return Ok(());
        }
        run("launchctl", &["bootout", target])?;
        let deadline = Instant::now() + Duration::from_secs(30);
        while loaded()? {
            if Instant::now() >= deadline {
                return Err("launchd did not finish unloading the service".into());
            }
            thread::sleep(Duration::from_millis(100));
        }
        Ok(())
    }
}

#[cfg(windows)]
mod platform {
    use super::Error;
    use std::{os::windows::process::CommandExt, process::Command};

    /// Keeps PowerShell from opening a console window when the app is
    /// opened from Explorer.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    pub fn install(enable: bool) -> Result<(), Box<dyn Error>> {
        // The running sender ends its session with the KVM before the
        // script ends its task. The script ends a sender that does not stop.
        if let Err(error) = super::windows::stop_sender() {
            eprintln!("ipkvm-os-services: {error}");
        }
        let output = Command::new("powershell.exe")
            .creation_flags(CREATE_NO_WINDOW)
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                include_str!("../../packaging/windows/service.ps1"),
            ])
            .env("OS_SERVICES_INSTALL", if enable { "1" } else { "0" })
            .env("OS_SERVICES_EXECUTABLE", std::env::current_exe()?)
            .output()?;
        if !output.status.success() {
            // PowerShell's first error line names the command and the cause.
            let stderr = String::from_utf8_lossy(&output.stderr);
            let reason = stderr
                .lines()
                .map(str::trim)
                .find(|line| !line.is_empty())
                .unwrap_or("no error output");
            return Err(format!("the scheduled task could not be changed: {reason}").into());
        }
        Ok(())
    }

    pub fn tell_user(text: &str, error: bool) {
        use winapi::um::winuser::{MessageBoxW, MB_ICONERROR, MB_ICONINFORMATION, MB_OK};
        let wide = |text: &str| -> Vec<u16> { text.encode_utf16().chain([0]).collect() };
        let icon = if error {
            MB_ICONERROR
        } else {
            MB_ICONINFORMATION
        };
        unsafe {
            MessageBoxW(
                std::ptr::null_mut(),
                wide(text).as_ptr(),
                wide("IP-KVM OS Services").as_ptr(),
                MB_OK | icon,
            );
        }
    }
}

/// The size at which the service log starts again. The log before it stays
/// beside it as `service.log.1`, so the two have at most twice this size.
#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
const LOG_LIMIT: u64 = 8 * 1024 * 1024;

/// Points standard output and error at the service log. A scheduled task has
/// no console, and the Unix supervisors collect the same streams.
#[cfg(windows)]
pub fn open_log() -> Result<(), Box<dyn Error>> {
    let path = log_path()?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let log = point_streams_at(&path)?;
    lock_logs().current = Some(log);
    Ok(())
}

/// The handles of the log that the streams write to and of the log before
/// it, which is `service.log.1`.
#[cfg(windows)]
struct Logs {
    current: Option<std::fs::File>,
    previous: Option<std::fs::File>,
}

#[cfg(windows)]
static LOGS: std::sync::Mutex<Logs> = std::sync::Mutex::new(Logs {
    current: None,
    previous: None,
});

#[cfg(windows)]
fn lock_logs() -> std::sync::MutexGuard<'static, Logs> {
    LOGS.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Keeps the service log under `LOG_LIMIT` for the life of the process.
/// Under systemd, Linux has no such log: the journal collects the streams
/// and limits itself.
#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
pub fn limit_log() {
    const INTERVAL: std::time::Duration = std::time::Duration::from_secs(600);
    std::thread::spawn(|| loop {
        if let Err(error) = start_log_again_if_large() {
            eprintln!("ipkvm-os-services: the log could not start again: {error}");
        }
        std::thread::sleep(INTERVAL);
    });
}

#[cfg(windows)]
fn start_log_again_if_large() -> Result<(), Box<dyn Error>> {
    let path = log_path()?;
    let mut logs = lock_logs();
    // A thread that wrote while the log started again could still hold the
    // handle of the log before it, so that handle stayed open. One check
    // later no thread writes to it. It closes here, before the rename, so
    // that the rename replaces a file that nothing holds open.
    logs.previous = None;
    if let Some(log) = start_log_again(&path, LOG_LIMIT, || point_streams_at(&path))? {
        logs.previous = logs.current.replace(log);
    }
    Ok(())
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn start_log_again_if_large() -> Result<(), Box<dyn Error>> {
    let path = log_path()?;
    // launchd, or the keeper on Linux, points the streams at the log. A run
    // from a terminal keeps the streams it has.
    if stream_is(libc::STDERR_FILENO, &path) {
        let streams = [libc::STDOUT_FILENO, libc::STDERR_FILENO];
        start_log_again(&path, LOG_LIMIT, || point_streams_at(&path, &streams))?;
    }
    Ok(())
}

/// Moves a log of more than `limit` bytes aside and calls `reopen`, which
/// points the streams at a new log; None when the log is not that large.
/// When `reopen` fails, the streams still write to the old log, so it gets
/// its name back and the next check tries again.
#[cfg(any(windows, target_os = "macos", target_os = "linux", test))]
fn start_log_again<T>(
    log: &std::path::Path,
    limit: u64,
    reopen: impl FnOnce() -> std::io::Result<T>,
) -> std::io::Result<Option<T>> {
    if !set_log_aside(log, limit)? {
        return Ok(None);
    }
    match reopen() {
        Ok(opened) => Ok(Some(opened)),
        Err(error) => {
            std::fs::rename(previous_log(log), log)?;
            Err(error)
        }
    }
}

/// The name of the log before the current one.
#[cfg(any(windows, target_os = "macos", target_os = "linux", test))]
fn previous_log(log: &std::path::Path) -> std::path::PathBuf {
    let mut previous = log.as_os_str().to_owned();
    previous.push(".1");
    previous.into()
}

/// Moves a log of more than `limit` bytes to its `.1` name, in place of the
/// log before it; true when it moved. The streams that write to it can stay
/// open.
#[cfg(any(windows, target_os = "macos", target_os = "linux", test))]
fn set_log_aside(log: &std::path::Path, limit: u64) -> std::io::Result<bool> {
    match std::fs::metadata(log) {
        Ok(metadata) if metadata.len() > limit => {}
        Ok(_) => return Ok(false),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    }
    std::fs::rename(log, previous_log(log))?;
    Ok(true)
}

/// Opens the log and makes it the standard output and error. The streams
/// use the handle of the file that this returns.
#[cfg(windows)]
fn point_streams_at(log: &std::path::Path) -> std::io::Result<std::fs::File> {
    use std::{fs::File, os::windows::io::AsRawHandle};
    use winapi::um::{
        processenv::SetStdHandle,
        winbase::{STD_ERROR_HANDLE, STD_OUTPUT_HANDLE},
    };
    let file = File::options().create(true).append(true).open(log)?;
    for stream in [STD_ERROR_HANDLE, STD_OUTPUT_HANDLE] {
        if unsafe { SetStdHandle(stream, file.as_raw_handle() as _) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(file)
}

/// Opens the log and makes it the file behind each of `streams`.
#[cfg(any(target_os = "macos", target_os = "linux", all(unix, test)))]
fn point_streams_at(log: &std::path::Path, streams: &[std::os::fd::RawFd]) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    let file = std::fs::File::options()
        .create(true)
        .append(true)
        .open(log)?;
    for &stream in streams {
        if unsafe { libc::dup2(file.as_raw_fd(), stream) } < 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Whether `stream` writes to `file`.
#[cfg(any(target_os = "macos", target_os = "linux", all(unix, test)))]
fn stream_is(stream: std::os::fd::RawFd, file: &std::path::Path) -> bool {
    use std::os::{fd::FromRawFd, unix::fs::MetadataExt};
    // The stream stays open: this only looks at it.
    let stream = std::mem::ManuallyDrop::new(unsafe { std::fs::File::from_raw_fd(stream) });
    match (stream.metadata(), std::fs::metadata(file)) {
        (Ok(stream), Ok(file)) => stream.dev() == file.dev() && stream.ino() == file.ino(),
        _ => false,
    }
}

#[cfg(windows)]
fn log_path() -> Result<std::path::PathBuf, Box<dyn Error>> {
    Ok(dirs::data_local_dir()
        .ok_or("no local data directory")?
        .join("IP-KVM OS Services/logs/service.log"))
}

#[cfg(target_os = "macos")]
fn log_path() -> Result<std::path::PathBuf, Box<dyn Error>> {
    Ok(dirs::home_dir()
        .ok_or("no home directory")?
        .join("Library/Logs/IP-KVM OS Services/service.log"))
}

/// The log on a computer without systemd.
#[cfg(target_os = "linux")]
fn log_path() -> Result<std::path::PathBuf, Box<dyn Error>> {
    keeper::log_path()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_large_log_moves_aside_while_a_stream_writes_to_it() {
        use std::io::Write;
        let dir = std::env::temp_dir().join(format!("log-aside-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (log, previous) = (dir.join("service.log"), dir.join("service.log.1"));
        assert!(!set_log_aside(&log, 5).unwrap());
        std::fs::write(&previous, "the log before").unwrap();
        let mut stream = std::fs::File::options()
            .create(true)
            .append(true)
            .open(&log)
            .unwrap();
        stream.write_all(b"before").unwrap();
        assert!(!set_log_aside(&log, 6).unwrap());
        assert!(set_log_aside(&log, 5).unwrap());
        assert!(!log.exists());
        assert_eq!(std::fs::read(&previous).unwrap(), b"before");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_log_whose_new_file_does_not_open_keeps_its_name() {
        let dir = std::env::temp_dir().join(format!("log-again-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (log, previous) = (dir.join("service.log"), dir.join("service.log.1"));
        std::fs::write(&log, "before").unwrap();
        let no_handle = || Err::<(), _>(std::io::Error::other("no file handle is free"));
        assert!(start_log_again(&log, 5, no_handle).is_err());
        assert_eq!(std::fs::read(&log).unwrap(), b"before");
        assert!(!previous.exists());
        // The next check finds the large log under its name and tries again.
        assert_eq!(start_log_again(&log, 5, || Ok(1)).unwrap(), Some(1));
        assert_eq!(std::fs::read(&previous).unwrap(), b"before");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn the_streams_follow_the_log_that_starts_again() {
        use std::{io::Write, os::fd::AsRawFd};
        let dir = std::env::temp_dir().join(format!("log-streams-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (log, previous) = (dir.join("service.log"), dir.join("service.log.1"));
        let mut stream = std::fs::File::options()
            .create(true)
            .append(true)
            .open(&log)
            .unwrap();
        stream.write_all(b"before").unwrap();
        assert!(stream_is(stream.as_raw_fd(), &log));
        assert!(set_log_aside(&log, 5).unwrap());
        assert!(stream_is(stream.as_raw_fd(), &previous));
        point_streams_at(&log, &[stream.as_raw_fd()]).unwrap();
        stream.write_all(b"after").unwrap();
        assert!(stream_is(stream.as_raw_fd(), &log));
        assert_eq!(std::fs::read(&previous).unwrap(), b"before");
        assert_eq!(std::fs::read(&log).unwrap(), b"after");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn action(args: &[&str]) -> Result<Action, String> {
        let args: Vec<OsString> = args.iter().map(OsString::from).collect();
        Action::from_args(&args)
    }

    #[test]
    fn a_flag_is_the_only_argument() {
        assert_eq!(action(&[]), Ok(Action::Open));
        assert_eq!(action(&["--run"]), Ok(Action::Run));
        #[cfg(windows)]
        assert_eq!(action(&["--stop"]), Ok(Action::Stop));
        #[cfg(not(windows))]
        assert_eq!(action(&["--stop"]), Err(USAGE.into()));
        assert_eq!(action(&["--install-service"]), Ok(Action::InstallService));
        assert_eq!(
            action(&["--uninstall-service"]),
            Ok(Action::UninstallService)
        );
        assert_eq!(action(&["--install-service", "extra"]), Err(USAGE.into()));
        assert_eq!(action(&["--invalid"]), Err(USAGE.into()));
    }

    #[test]
    fn kvm_flags_take_a_fingerprint() {
        let kvm = "34:75:0F:98:BD:59:FC:FC";
        let fingerprint = Fingerprint::parse(kvm).unwrap();
        assert_eq!(
            action(&["--approve-kvm", kvm]),
            Ok(Action::ApproveKvm(fingerprint))
        );
        assert_eq!(
            action(&["--forget-kvm", kvm]),
            Ok(Action::ForgetKvm(fingerprint))
        );
        assert!(action(&["--approve-kvm", "34:75"]).is_err());
        assert_eq!(action(&["--approve-kvm"]), Err(USAGE.into()));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_access_flags_select_their_installers() {
        assert_eq!(
            action(&[linux::USB_ACCESS_FLAG]),
            Ok(Action::InstallUsbAccess)
        );
        assert_eq!(
            action(&[linux::CAPTURE_ACCESS_FLAG]),
            Ok(Action::InstallCaptureAccess)
        );
        assert_eq!(action(&[linux::ACCESS_FLAG]), Ok(Action::InstallAccess));
    }
}
