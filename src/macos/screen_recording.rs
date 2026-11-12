//! macOS grants screen capture to a code signature, and a build signed
//! differently from the one System Settings lists is refused while the
//! switch reads as on. The capture learns this only when its stream starts,
//! after the USB session is open, so the session is gated on TCC's answer.

use core_graphics::access::ScreenCaptureAccess;
use std::sync::Once;

/// True when TCC grants this process screen capture. Answered per call, so a
/// waiting sender notices a grant by asking again.
pub fn granted() -> bool {
    ScreenCaptureAccess.preflight()
}

/// Asks macOS for the grant. An app TCC has no row for gets the system
/// prompt; an app with a row, granted to another build or denied, gets
/// nothing, so one ask per process is all that can have an effect.
pub fn request_once() {
    static REQUESTED: Once = Once::new();
    REQUESTED.call_once(|| {
        ScreenCaptureAccess.request();
    });
}
