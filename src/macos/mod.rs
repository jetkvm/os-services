//! macOS support: the AppKit event loop, the Screen Recording grant, display
//! capture through ScreenCaptureKit, and the Objective-C helpers in this
//! directory.

pub mod screen_capture;
pub mod screen_recording;

use std::sync::OnceLock;

extern "C" {
    fn guest_appkit_initialize();
    fn guest_appkit_run();
    fn guest_appkit_stop();
    fn guest_show_message(text: *const std::ffi::c_char, error: bool);
}

/// Shows `text` in an alert and waits until the user closes it.
pub fn show_message(text: &str, error: bool) {
    let text = std::ffi::CString::new(text.replace('\0', "")).unwrap_or_default();
    unsafe { guest_show_message(text.as_ptr(), error) };
}

static ON_REOPEN: OnceLock<fn()> = OnceLock::new();

/// Called on the main thread when the user opens the app while it runs.
#[no_mangle]
extern "C" fn guest_reopened() {
    if let Some(on_reopen) = ON_REOPEN.get() {
        on_reopen();
    }
}

/// Runs `service` on a new thread while AppKit runs on the main thread, and
/// returns when the service does. AppKit processes display events on the
/// main thread even for an application without windows; without it,
/// CoreGraphics can keep a stale display mode across a hotplug and report
/// logical pixels as capture pixels. Opening the app while the service runs
/// calls `on_reopen`.
pub fn run_with_event_loop(on_reopen: fn(), service: impl FnOnce() + Send + 'static) {
    let _ = ON_REOPEN.set(on_reopen);
    unsafe { guest_appkit_initialize() };
    let service = std::thread::spawn(move || {
        struct StopAppKit;
        impl Drop for StopAppKit {
            fn drop(&mut self) {
                unsafe { guest_appkit_stop() };
            }
        }
        let _stop = StopAppKit;
        service();
    });
    unsafe { guest_appkit_run() };
    if let Err(panic) = service.join() {
        std::panic::resume_unwind(panic);
    }
}
