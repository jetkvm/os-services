//! IP-KVM OS Services: the service on the computer a KVM is plugged into.
//! It streams the screen, shares the clipboard, opens shells and receives
//! files, all over the KVM's USB link.
#![cfg_attr(
    all(not(test), not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

mod approval;
#[cfg(target_os = "macos")]
mod macos;
mod protocol;
mod services;
mod session;
mod supervisor;
#[cfg(target_os = "linux")]
mod usb_access;
mod video;

use std::{
    ffi::OsString,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};
use supervisor::{update::Installation, Action};

/// How long the sender waits before it looks for the KVM again after a
/// session ends with an error.
const RECONNECT_DELAY: Duration = Duration::from_secs(2);

fn main() {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    #[cfg(target_os = "linux")]
    if args
        .first()
        .is_some_and(|arg| arg.as_os_str() == video::kms::capture_test::FLAG)
    {
        supervisor::linux::announce_bundled_paths();
        let args: Vec<&str> = args
            .iter()
            .map(|arg| arg.to_str().unwrap_or_default())
            .collect();
        std::process::exit(video::kms::capture_test::run(&args));
    }
    let action = match Action::from_args(&args) {
        Ok(action) => action,
        Err(usage) => {
            eprintln!("ipkvm-os-services: {usage}");
            std::process::exit(1);
        }
    };
    if action != Action::Run {
        std::process::exit(action.perform());
    }
    #[cfg(windows)]
    if let Err(error) = supervisor::open_log() {
        eprintln!("ipkvm-os-services: {error}");
        std::process::exit(1);
    }
    #[cfg(any(windows, target_os = "macos"))]
    supervisor::limit_log();
    #[cfg(target_os = "linux")]
    {
        // Without systemd, this process keeps the service running, and the
        // service is a child process.
        if supervisor::keeper::needed() {
            std::process::exit(supervisor::keeper::run());
        }
        if supervisor::keeper::kept() {
            supervisor::limit_log();
        }
        supervisor::leave_appimage();
        supervisor::linux::announce_bundled_paths();
    }

    let shutdown = Arc::new(AtomicBool::new(false));
    let handler_flag = shutdown.clone();
    ctrlc::set_handler(move || handler_flag.store(true, Ordering::Relaxed))
        .expect("install the shutdown handler");
    #[cfg(windows)]
    supervisor::windows::watch_stop_requests(&shutdown);
    #[cfg(target_os = "linux")]
    usb_access::offer_helper_update(shutdown.clone());
    #[cfg(target_os = "macos")]
    macos::run_with_event_loop(supervisor::reopened, move || serve(&shutdown));
    #[cfg(not(target_os = "macos"))]
    serve(&shutdown);
}

/// Serves one KVM session after another until the process is asked to stop,
/// or until an update from the KVM replaces it.
fn serve(shutdown: &AtomicBool) {
    // The log names the build that runs. The session.info JSON alone does
    // not keep a short version such as 0.4.2 in the executable: the
    // compiler writes those few bytes with immediate stores, and the e2e
    // suite finds the running build by the version in its executable.
    eprintln!("ipkvm-os-services {} running", protocol::SENDER_VERSION);
    let installation = Installation::current();
    if installation.is_none() {
        eprintln!("ipkvm-os-services: this installation cannot update itself from the KVM");
    }
    #[cfg(target_os = "linux")]
    let mut usb_access = usb_access::UsbAccess::default();
    let mut log = ErrorLog::default();
    while !shutdown.load(Ordering::Relaxed) {
        let mut error = match session::run(shutdown, installation.as_ref()) {
            Ok(None) => continue,
            Ok(Some(update)) => match update.install() {
                Ok(()) => std::process::exit(supervisor::update::restart_status()),
                Err(error) => {
                    eprintln!("ipkvm-os-services: update not installed: {error}");
                    continue;
                }
            },
            Err(error) => error,
        };
        #[cfg(target_os = "linux")]
        if error.is::<protocol::UsbAccessDenied>() {
            log.write(&error.to_string(), Instant::now());
            if !usb_access.offer(shutdown) {
                sleep_unless_stopped(shutdown, RECONNECT_DELAY);
            }
            continue;
        }
        if let Some(protocol::NotApproved(kvm)) = error.downcast_ref() {
            match approval::wait_for_approval(kvm, shutdown) {
                Ok(()) => continue,
                Err(failure) => error = failure,
            }
        }
        log.write(&error.to_string(), Instant::now());
        sleep_unless_stopped(shutdown, RECONNECT_DELAY);
    }
}

/// The errors that end an attempt to serve a KVM. Without a KVM the same
/// error comes back every `RECONNECT_DELAY`, and the log has it one time.
#[derive(Default)]
struct ErrorLog {
    last: Option<(String, Instant)>,
}

impl ErrorLog {
    /// An error counts as the one before it while it keeps coming back
    /// within this time. After a session that ran for longer, the log says
    /// it again.
    const REPEAT: Duration = Duration::from_secs(10);

    fn write(&mut self, error: &str, now: Instant) {
        if !self.repeats(error, now) {
            eprintln!("ipkvm-os-services: {error}");
        }
    }

    fn repeats(&mut self, error: &str, now: Instant) -> bool {
        let repeats = self
            .last
            .as_ref()
            .is_some_and(|(last, at)| last == error && now.duration_since(*at) < Self::REPEAT);
        self.last = Some((error.to_owned(), now));
        repeats
    }
}

fn sleep_unless_stopped(shutdown: &AtomicBool, duration: Duration) {
    let deadline = Instant::now() + duration;
    while !shutdown.load(Ordering::Relaxed) {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        thread::sleep(remaining.min(session::CONTROL_POLL));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_error_that_keeps_coming_back_is_logged_one_time() {
        let mut log = ErrorLog::default();
        let start = Instant::now();
        let at = |seconds| start + Duration::from_secs(seconds);
        assert!(!log.repeats("no KVM", at(0)));
        // The error comes back every two seconds for as long as no KVM is
        // attached.
        for seconds in (2..60).step_by(2) {
            assert!(log.repeats("no KVM", at(seconds)));
        }
        assert!(!log.repeats("handshake failed", at(60)));
        assert!(!log.repeats("no KVM", at(62)));
        // The same error after a session that ran for a minute.
        assert!(!log.repeats("no KVM", at(122)));
    }
}
