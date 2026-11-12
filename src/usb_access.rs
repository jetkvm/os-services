//! The first run on Linux. Without the USB access rule, the desktop user may
//! not open the KVM's USB device, and a sender that a file manager started
//! has no terminal to say so. The sender asks in a dialog instead, and
//! installs the rule and the capture helper through pkexec, which asks for
//! an administrator's password. After an update from the KVM, the sender
//! asks the same way to replace a capture helper that is older than its own.

use crate::{approval::Dialog, protocol, supervisor::linux};
use std::{
    collections::HashSet,
    ffi::OsString,
    io,
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::Duration,
};

/// How often a waiting dialog or install checks for its answer, and for the
/// KVM being attached or detached.
const POLL: Duration = Duration::from_millis(250);

/// A question whose yes runs the install.
struct Question {
    title: &'static str,
    text: &'static str,
    yes: &'static str,
    /// The button that declines, when the user has that choice.
    no: Option<&'static str>,
    /// What the log calls the thing the user allows.
    subject: &'static str,
}

const USB_ACCESS: Question = Question {
    title: "Allow USB access?",
    text: "IP-KVM OS Services cannot open the KVM's USB device. This computer \
does not allow it yet.\n\n\
To allow it, select Allow USB Access and enter an administrator password. This also installs \
the screen capture helper. The KVM can show your screen after you log out and log in again.",
    yes: "Allow USB Access",
    no: Some("Not Now"),
    subject: "USB access",
};
const HELPER_UPDATE: Question = Question {
    title: "Update the screen capture helper",
    text: "IP-KVM OS Services was updated, and the screen capture helper on this computer is \
older than the service needs. Until the helper is updated, the KVM shows its HDMI capture.\n\n\
To update it, select Update Helper and enter an administrator password.",
    yes: "Update Helper",
    // The service does not capture with the older helper, so there is
    // nothing to choose. A closed dialog comes back when the service starts
    // again.
    no: None,
    subject: "the capture helper update",
};
const WARNING_TITLE: &str = "USB access not allowed";

/// pkexec's exit codes when it does not run the program.
const PKEXEC_DISMISSED: i32 = 126;
const PKEXEC_NOT_AUTHORIZED: i32 = 127;

/// The prompt's state across the sender's connection attempts.
#[derive(Default)]
pub struct UsbAccess {
    /// The KVMs attached when the user said no or the install failed. The
    /// sender does not ask again until a KVM is attached or detached.
    declined: Option<HashSet<nusb::DeviceId>>,
}

impl UsbAccess {
    /// Asks the user to allow USB access and installs it; true when it was
    /// installed, so the sender connects again at once. It does not ask when
    /// the rule is installed, or when the user said no to the KVMs that are
    /// attached now; the sender then keeps trying to connect, so access
    /// given another way, such as a terminal command, takes effect.
    pub fn offer(&mut self, shutdown: &AtomicBool) -> bool {
        let attached = protocol::attached_interfaces();
        if linux::usb_rule_installed() || self.declined.as_ref() == Some(&attached) {
            return false;
        }
        self.declined = None;
        let Some(program) = installer(std::env::var_os("APPIMAGE"), std::env::current_exe().ok())
        else {
            return false;
        };
        let command = terminal_command(&program);
        let waiting = || {
            !shutdown.load(Ordering::Relaxed)
                && protocol::attached_interfaces() == attached
                && !linux::usb_rule_installed()
        };
        match ask(&USB_ACCESS, &command, waiting) {
            Some(true) => {}
            Some(false) => {
                self.declined = Some(attached);
                return false;
            }
            None => return false,
        }
        match install(&program, shutdown) {
            Ok(()) => {
                eprintln!("ipkvm-os-services: installed USB access");
                true
            }
            // The capture part failed after the rule went in: the KVM
            // connects, and video reports the missing helper.
            Err(reason) if linux::usb_rule_installed() => {
                eprintln!("ipkvm-os-services: installed USB access, but not capture: {reason}");
                true
            }
            Err(reason) => {
                eprintln!("ipkvm-os-services: USB access not installed: {reason}; run: {command}");
                show_warning(&warning_text(&reason, &command), attached.clone());
                self.declined = Some(attached);
                false
            }
        }
    }
}

/// Shows the warning with the terminal command while the session goes on.
/// It closes when the user closes it, when the rule is installed, or when a
/// KVM is attached or detached.
fn show_warning(text: &str, attached: HashSet<nusb::DeviceId>) {
    let mut warning = Dialog::spawn(vec![zenity("--warning", WARNING_TITLE, text, &[])]);
    thread::spawn(move || {
        while warning.is_open()
            && !linux::usb_rule_installed()
            && protocol::attached_interfaces() == attached
        {
            // Reaps the dialog once the user closes it.
            let _ = warning.approved();
            thread::sleep(POLL);
        }
    });
}

/// Asks the question: Some(true) when the user allows, Some(false) when the
/// user says no or no dialog opens, and None when `waiting` ends first.
fn ask(question: &Question, command: &str, waiting: impl Fn() -> bool) -> Option<bool> {
    let subject = question.subject;
    let mut dialog = Dialog::spawn(vec![match question.no {
        Some(no) => zenity(
            "--question",
            question.title,
            question.text,
            &["--ok-label", question.yes, "--cancel-label", no],
        ),
        None => zenity(
            "--info",
            question.title,
            question.text,
            &["--ok-label", question.yes],
        ),
    }]);
    if !dialog.is_open() {
        eprintln!(
            "ipkvm-os-services: no dialog program found to ask for {subject}; run: {command}"
        );
        return Some(false);
    }
    match answer(&mut dialog, waiting) {
        Ok(Some(false)) => {
            eprintln!("ipkvm-os-services: {subject} not allowed; to allow it, run: {command}");
            Some(false)
        }
        Ok(answer) => answer,
        Err(error) => {
            eprintln!("ipkvm-os-services: the dialog for {subject} failed: {error}");
            Some(false)
        }
    }
}

/// Asks, one time, to replace an installed capture helper that is older
/// than the one this build carries (see `linux::helper`). The service does
/// not capture with the older helper, and its other services go on while
/// the dialog is open. Each start of the service asks again, and opening
/// the app starts the service.
pub fn offer_helper_update(shutdown: Arc<AtomicBool>) {
    if linux::helper() != linux::Helper::Older {
        return;
    }
    let Some(program) = installer(std::env::var_os("APPIMAGE"), std::env::current_exe().ok())
    else {
        return;
    };
    let command = terminal_command(&program);
    eprintln!(
        "ipkvm-os-services: the installed capture helper is older than this service; \
         to update it, run: {command}"
    );
    thread::spawn(move || {
        let waiting =
            || !shutdown.load(Ordering::Relaxed) && linux::helper() == linux::Helper::Older;
        if ask(&HELPER_UPDATE, &command, waiting) != Some(true) {
            return;
        }
        match install(&program, &shutdown) {
            Ok(()) => eprintln!("ipkvm-os-services: updated the capture helper"),
            Err(reason) => {
                eprintln!("ipkvm-os-services: capture helper not updated: {reason}; run: {command}")
            }
        }
    });
}

/// The program pkexec runs. pkexec needs an absolute path. An AppImage runs
/// from a temporary mount, so it is the image, which may be on a read-only
/// disc.
fn installer(appimage: Option<OsString>, executable: Option<PathBuf>) -> Option<PathBuf> {
    appimage
        .map(PathBuf::from)
        .filter(|image| image.is_absolute())
        .or(executable)
}

/// The command that does the same install from a terminal.
fn terminal_command(program: &Path) -> String {
    format!(
        "sudo {} {}",
        shell_quote(&program.to_string_lossy()),
        linux::ACCESS_FLAG
    )
}

/// `text` as one word for a POSIX shell.
fn shell_quote(text: &str) -> String {
    let plain = |byte: u8| byte.is_ascii_alphanumeric() || b"/._-+:,@%=".contains(&byte);
    if !text.is_empty() && text.bytes().all(plain) {
        return text.to_owned();
    }
    format!("'{}'", text.replace('\'', r"'\''"))
}

fn warning_text(reason: &str, command: &str) -> String {
    format!(
        "IP-KVM OS Services could not allow USB access: {reason}.\n\n\
         To allow it, run this command in a terminal:\n\n{command}"
    )
}

/// A zenity dialog. The text is plain: a path can hold characters that are
/// Pango markup.
pub fn zenity(kind: &str, title: &str, text: &str, labels: &[&str]) -> Command {
    let mut zenity = Command::new("zenity");
    zenity.args([kind, "--no-markup", "--width", "460"]);
    zenity.args(["--title", title, "--text", text]);
    zenity.args(labels);
    zenity
}

/// The user's answer: None when `waiting` ended first.
fn answer(dialog: &mut Dialog, waiting: impl Fn() -> bool) -> io::Result<Option<bool>> {
    while waiting() {
        if dialog.approved()? {
            return Ok(Some(true));
        }
        if !dialog.is_open() {
            return Ok(Some(false));
        }
        thread::sleep(POLL);
    }
    Ok(None)
}

/// Runs the install as root through pkexec. Its output goes to the log.
fn install(program: &Path, shutdown: &AtomicBool) -> Result<(), String> {
    let mut pkexec = Command::new("pkexec")
        .arg(program)
        .arg(linux::ACCESS_FLAG)
        .stdin(Stdio::null())
        .spawn()
        .map_err(|error| format!("could not run pkexec: {error}"))?;
    loop {
        if let Some(status) = pkexec.try_wait().map_err(|error| error.to_string())? {
            return install_result(status);
        }
        if shutdown.load(Ordering::Relaxed) {
            let _ = pkexec.kill();
            let _ = pkexec.wait();
            return Err("the service stopped".into());
        }
        thread::sleep(POLL);
    }
}

fn install_result(status: ExitStatus) -> Result<(), String> {
    match status.code() {
        Some(0) => Ok(()),
        Some(PKEXEC_DISMISSED) => Err("the password dialog was closed".into()),
        Some(PKEXEC_NOT_AUTHORIZED) => {
            Err("no administrator authorized it, or no password dialog could open".into())
        }
        _ => Err(format!(
            "the install failed ({status}); the log has the reason"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkexec_runs_the_appimage_rather_than_its_mount() {
        let mounted = Some(PathBuf::from(
            "/tmp/.mount_IP-KVMx1/usr/bin/ipkvm-os-services",
        ));
        let image = "/media/user/OS Services/IP-KVM-OS-Services_0.4.2_linux-x86_64.AppImage";
        assert_eq!(
            installer(Some(image.into()), mounted.clone()),
            Some(PathBuf::from(image))
        );
        assert_eq!(installer(None, mounted.clone()), mounted);
        // pkexec refuses a relative path.
        assert_eq!(
            installer(Some("x.AppImage".into()), mounted.clone()),
            mounted
        );
        assert_eq!(installer(None, None), None);
    }

    #[test]
    fn the_terminal_command_survives_a_shell() {
        assert_eq!(
            terminal_command(Path::new("/usr/bin/ipkvm-os-services")),
            "sudo /usr/bin/ipkvm-os-services --install-linux-access"
        );
        assert_eq!(
            terminal_command(Path::new("/media/user/OS Services/x.AppImage")),
            "sudo '/media/user/OS Services/x.AppImage' --install-linux-access"
        );
        assert_eq!(shell_quote("/home/o'brien/x"), r"'/home/o'\''brien/x'");
        assert_eq!(shell_quote("$(reboot)"), "'$(reboot)'");
        assert_eq!(shell_quote(""), "''");
    }
}
