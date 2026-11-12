//! The displays capture can bind to. Capture and the KVM's display picker
//! use the same display names on each platform: the GDI device name on
//! Windows, the CGDirectDisplayID on macOS, and "card-connector" DRM names
//! on Linux.

#[cfg(target_os = "linux")]
mod drm;
mod layout;
#[cfg(target_os = "macos")]
mod macos;
mod select;
#[cfg(windows)]
mod windows;

#[cfg(target_os = "linux")]
pub use drm::Display;
pub use layout::{adopted_size, apply_layout, apply_mode, SETS_MODE};
#[cfg(target_os = "macos")]
pub use macos::Display;
pub use select::DisplaySelector;
#[cfg(windows)]
pub use windows::{Com, Display, Output};

use std::io;

/// The manufacturer id "JET" in the EDID the KVM presents, as EDID bytes
/// 8 and 9 store it.
pub const JETKVM_EDID_VENDOR: u16 = 0x28b4;

/// The online display with this name.
pub fn find_online(name: &str) -> io::Result<Option<Display>> {
    Ok(Display::all()?
        .into_iter()
        .find(|display| display.name() == name && display.is_online()))
}

/// The names of the online displays, for the KVM's display picker.
pub fn online_names() -> Vec<String> {
    Display::all()
        .unwrap_or_default()
        .into_iter()
        .filter(|display| display.is_online())
        .map(|display| display.name())
        .collect()
}

/// One line describing every display, for the log.
pub fn describe(displays: &[Display]) -> String {
    displays
        .iter()
        .map(|display| {
            format!(
                "{}={}x{} origin={:?} primary={} online={}",
                display.name(),
                display.width(),
                display.height(),
                display.origin(),
                display.is_primary(),
                display.is_online()
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
}
