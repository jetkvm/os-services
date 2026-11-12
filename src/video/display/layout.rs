//! The mode and the mirror or extend placement of the KVM display. The KVM
//! names the resolution in the EDID it presents, as the preferred timing. A
//! desktop adopts that timing only for a monitor it has not seen, and gives
//! a monitor it knows the mode it had. A sender that sets the mode says so
//! to the KVM, which then presents one monitor that the desktop knows
//! again; for any other sender the KVM presents a new monitor each time.
//! Topology is the part an EDID cannot express, and it matters only while
//! another display is connected.

use super::{find_online, online_names, select};
use crate::protocol::{DisplayLayout, VideoConfig};
use std::error::Error;

/// Whether this sender sets the mode of the KVM display. It tells the KVM
/// in `video.capabilities`.
pub const SETS_MODE: bool = platform::SETS_MODE;

/// Gives the KVM display the mode with the size in `config`, when it has
/// another. The stream calls it for a short time after the KVM applied its
/// settings, so a mode that the user picks on the computer stays until the
/// next apply.
///
/// Only the display that the KVM presents for `config` is changed: the one
/// KVM display that has that size as its preferred mode, also when capture
/// uses another display. Other displays keep their mode, and so does the
/// KVM display of before, which a desktop can still list for a moment. When
/// several KVM displays prefer that size, the sender cannot tell which one
/// this KVM presents, and sets no mode. With the mirror layout and another
/// display, the KVM display takes the size of the display that it mirrors,
/// and no mode is set.
pub fn apply_mode(config: VideoConfig) -> Result<(), Box<dyn Error>> {
    if config.layout == DisplayLayout::Mirror && online_names().len() > 1 {
        return Ok(());
    }
    let size = (config.width, config.height);
    let mut presented = select::kvm_display_names()
        .into_iter()
        .filter(|name| platform::preferred_size(name) == Some(size));
    let (Some(name), None) = (presented.next(), presented.next()) else {
        return Ok(());
    };
    let current =
        find_online(&name)?.map(|display| (display.width() as u32, display.height() as u32));
    if current.is_some_and(|current| current != size) {
        platform::set_mode(&name, size)?;
        eprintln!("KVM display mode set: {}x{}", size.0, size.1);
    }
    Ok(())
}

/// The size that the desktop gives the KVM display `display_name` by itself.
/// On a platform where the sender sets no mode, a desktop adopts the
/// preferred timing of a monitor it has not seen, which the KVM presents at
/// each start of service video with the size in `config` as that timing.
/// Mutter applies that mode a moment after the connector reports the
/// monitor, which until then still has the mode of the monitor before. None
/// for any other display, with the mirror layout beside another display
/// (which takes the mirrored display's size), and where the sender sets the
/// mode.
pub fn adopted_size(display_name: &str, config: VideoConfig) -> Option<(usize, usize)> {
    if SETS_MODE
        || (config.layout == DisplayLayout::Mirror && online_names().len() > 1)
        || !select::kvm_display_names().contains(display_name)
    {
        return None;
    }
    Some((config.width as usize, config.height as usize))
}

/// Places the display named `display_name` next to, or as a mirror of, the
/// primary other display.
pub fn apply_layout(display_name: &str, config: VideoConfig) -> Result<(), Box<dyn Error>> {
    platform::apply_layout(display_name, config.layout)
}

/// X11 placement through xrandr. GNOME's mutter re-applies its stored layout
/// when it re-enumerates displays, so on GNOME the placement lasts until
/// then.
#[cfg(target_os = "linux")]
mod platform {
    use super::*;
    use crate::video::display::Display;
    use std::process::Command;

    /// The Linux sender sets no mode. The KVM then presents a monitor that
    /// the desktop has not seen, and the desktop adopts its preferred
    /// timing. A mode set from outside competes with the configuration that
    /// GNOME's mutter stores for a monitor, and mutter 46 to 50 can end the
    /// desktop session when it re-applies a configuration while the KVM
    /// plugs the monitor again (GNOME mutter issue 3170).
    pub const SETS_MODE: bool = false;

    pub fn preferred_size(_display_name: &str) -> Option<(u32, u32)> {
        None
    }

    pub fn set_mode(_display_name: &str, _size: (u32, u32)) -> Result<(), Box<dyn Error>> {
        Ok(())
    }

    pub fn apply_layout(display_name: &str, layout: DisplayLayout) -> Result<(), Box<dyn Error>> {
        let displays = Display::all()?;
        let Some(reference) = displays
            .iter()
            .filter(|display| display.name() != display_name && display.is_online())
            .min_by_key(|display| !display.is_primary())
        else {
            return Ok(());
        };
        let target = displays
            .iter()
            .find(|display| display.name() == display_name)
            .ok_or("selected display is unavailable")?;
        for connector in [target.connector(), reference.connector()] {
            if displays
                .iter()
                .filter(|d| d.connector() == connector)
                .count()
                != 1
            {
                return Err("X11 layout has ambiguous connector identifiers across GPUs".into());
            }
        }
        let properties = Command::new("xrandr").arg("--props").output()?;
        let properties = String::from_utf8_lossy(&properties.stdout);
        let target_output = xrandr_output(&properties, target.connector())
            .ok_or("desktop does not support X11 display layout changes")?;
        let reference_output = xrandr_output(&properties, reference.connector())
            .ok_or("reference display has no X11 output")?;
        let mut command = Command::new("xrandr");
        command.args(["--output", target_output]);
        match layout {
            DisplayLayout::Mirror => {
                let size = format!("{}x{}", reference.width(), reference.height());
                command.args(["--same-as", reference_output, "--scale-from", &size]);
            }
            DisplayLayout::Extend => {
                command.args(["--right-of", reference_output]);
            }
        }
        let output = command.output().map_err(|error| {
            format!("could not run xrandr; display layout requires X11: {error}")
        })?;
        if !output.status.success() {
            let message = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            return Err(format!("xrandr could not place {display_name}: {message}").into());
        }
        Ok(())
    }

    /// The xrandr output whose CONNECTOR_ID property is `connector`, when
    /// exactly one is.
    pub(super) fn xrandr_output(properties: &str, connector: u32) -> Option<&str> {
        let mut output = None;
        let mut found = None;
        for line in properties.lines() {
            if !line.starts_with(char::is_whitespace) {
                output = line.split_whitespace().next();
                continue;
            }
            let id = line
                .trim()
                .strip_prefix("CONNECTOR_ID:")
                .and_then(|id| id.trim().parse::<u32>().ok());
            if id == Some(connector) {
                if found.is_some() {
                    return None;
                }
                found = output;
            }
        }
        found
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use super::*;
    use crate::video::display::JETKVM_EDID_VENDOR;
    use core_graphics::display::{
        kCGDisplayShowDuplicateLowResolutionModes, kDisplayModeDefaultFlag, CFArrayGetCount,
        CFArrayGetValueAtIndex, CFDictionaryRef, CFRelease, CGConfigureDisplayWithDisplayMode,
        CGConfigureOption, CGDisplay, CGDisplayConfigRef, CGDisplayCopyAllDisplayModes,
        CGDisplayModeGetIOFlags, CGDisplayModeGetPixelHeight, CGDisplayModeGetPixelWidth,
    };
    use core_graphics::sys::CGDisplayModeRef;
    use std::{ffi::c_void, ptr};

    pub const SETS_MODE: bool = true;

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        static kCFBooleanTrue: *const c_void;
        static kCFTypeDictionaryKeyCallBacks: [u8; 0];
        static kCFTypeDictionaryValueCallBacks: [u8; 0];
        fn CFDictionaryCreate(
            allocator: *const c_void,
            keys: *const *const c_void,
            values: *const *const c_void,
            count: isize,
            key_callbacks: *const c_void,
            value_callbacks: *const c_void,
        ) -> CFDictionaryRef;
    }

    /// The pixel size of the display's default mode.
    pub fn preferred_size(display_name: &str) -> Option<(u32, u32)> {
        with_default_mode(display_id(display_name).ok()?, |mode| unsafe {
            (
                CGDisplayModeGetPixelWidth(mode) as u32,
                CGDisplayModeGetPixelHeight(mode) as u32,
            )
        })
        .ok()
    }

    /// Sets the display's default mode, which `preferred_size` found to
    /// have this size. Permanently: macOS then gives the monitor this mode
    /// when it sees it again, and each change of a display's mode costs
    /// macOS, so the next presentations change nothing.
    pub fn set_mode(display_name: &str, _size: (u32, u32)) -> Result<(), Box<dyn Error>> {
        let display = CGDisplay::new(display_id(display_name)?);
        with_default_mode(display.id, |mode| {
            let config = display
                .begin_configuration()
                .map_err(|error| format!("could not begin macOS display configuration: {error}"))?;
            let error =
                unsafe { CGConfigureDisplayWithDisplayMode(config, display.id, mode, ptr::null()) };
            if error != 0 {
                let _ = display.cancel_configuration(&config);
                return Err(format!("could not set the macOS display mode: {error}"));
            }
            display
                .complete_configuration(&config, CGConfigureOption::ConfigurePermanently)
                .map_err(|error| format!("could not apply the macOS display mode: {error}"))
        })??;
        Ok(())
    }

    fn display_id(display_name: &str) -> Result<u32, String> {
        display_name
            .parse()
            .map_err(|_| format!("invalid macOS display id: {display_name}"))
    }

    /// Calls `use_mode` with the mode that macOS marks as the display's
    /// default: the EDID's preferred timing as macOS lays it out for a
    /// monitor it has not seen, 2x at 3840x2160 and 1x below. macOS lists
    /// the 2x mode only among the scaled modes, so those are asked for too.
    fn with_default_mode<T>(
        id: u32,
        use_mode: impl FnOnce(CGDisplayModeRef) -> T,
    ) -> Result<T, Box<dyn Error>> {
        let key: *const c_void = unsafe { kCGDisplayShowDuplicateLowResolutionModes }.cast();
        let modes = unsafe {
            let options = CFDictionaryCreate(
                ptr::null(),
                &key,
                &kCFBooleanTrue,
                1,
                ptr::addr_of!(kCFTypeDictionaryKeyCallBacks).cast(),
                ptr::addr_of!(kCFTypeDictionaryValueCallBacks).cast(),
            );
            let modes = CGDisplayCopyAllDisplayModes(id, options);
            if !options.is_null() {
                CFRelease(options.cast());
            }
            modes
        };
        if modes.is_null() {
            return Err("macOS lists no modes of the KVM display".into());
        }
        // The array owns its modes until it is released.
        let result = (0..unsafe { CFArrayGetCount(modes) })
            .map(|index| unsafe { CFArrayGetValueAtIndex(modes, index) } as CGDisplayModeRef)
            .find(|mode| unsafe { CGDisplayModeGetIOFlags(*mode) } & kDisplayModeDefaultFlag != 0)
            .map(use_mode);
        unsafe { CFRelease(modes.cast()) };
        result.ok_or_else(|| "macOS marks no default mode of the KVM display".into())
    }

    pub fn apply_layout(display_name: &str, layout: DisplayLayout) -> Result<(), Box<dyn Error>> {
        let target_id = display_name
            .parse::<u32>()
            .map_err(|_| format!("invalid macOS display id: {display_name}"))?;
        let active = CGDisplay::active_displays()
            .map_err(|error| format!("could not enumerate macOS displays: {error}"))?;
        let target = CGDisplay::new(target_id);
        if !active.contains(&target_id) || target.vendor_number() != u32::from(JETKVM_EDID_VENDOR) {
            return Err(format!("{display_name} is not an active KVM display").into());
        }
        let Some(reference) = active
            .into_iter()
            .filter(|id| *id != target_id)
            .map(CGDisplay::new)
            .min_by_key(|display| !display.is_main())
        else {
            return Ok(());
        };
        let config = target
            .begin_configuration()
            .map_err(|error| format!("could not begin macOS display configuration: {error}"))?;
        if let Err(error) = configure(&target, &reference, &config, layout) {
            let _ = target.cancel_configuration(&config);
            return Err(error.into());
        }
        target
            .complete_configuration(&config, CGConfigureOption::ConfigureForAppOnly)
            .map_err(|error| format!("could not apply macOS display configuration: {error}"))?;
        Ok(())
    }

    fn configure(
        target: &CGDisplay,
        reference: &CGDisplay,
        config: &CGDisplayConfigRef,
        layout: DisplayLayout,
    ) -> Result<(), String> {
        match layout {
            DisplayLayout::Mirror => target
                .configure_display_mirror_of_display(config, reference)
                .map_err(|error| format!("could not mirror macOS display: {error}")),
            DisplayLayout::Extend => {
                target
                    .configure_display_mirror_of_display(config, &CGDisplay::null_display())
                    .map_err(|error| format!("could not unmirror macOS display: {error}"))?;
                let bounds = reference.bounds();
                target
                    .configure_display_origin(
                        config,
                        (bounds.origin.x + bounds.size.width) as i32,
                        bounds.origin.y as i32,
                    )
                    .map_err(|error| format!("could not position macOS display: {error}"))
            }
        }
    }
}

/// Windows mode of one display, and topology: clone or extend across all
/// displays.
#[cfg(windows)]
mod platform {
    use super::*;
    use crate::video::display::Display;
    use std::{ffi::c_void, mem, ptr};
    use winapi::um::{
        wingdi::{
            DEVMODEW, DM_PELSHEIGHT, DM_PELSWIDTH, SDC_APPLY, SDC_TOPOLOGY_CLONE,
            SDC_TOPOLOGY_EXTEND,
        },
        winuser::{
            ChangeDisplaySettingsExW, GetSystemMetrics, CDS_UPDATEREGISTRY, DISP_CHANGE_SUCCESSFUL,
            SM_CMONITORS,
        },
    };

    pub const SETS_MODE: bool = true;

    /// The size of the mode that Windows reads as preferred from the EDID
    /// of the KVM's monitor on this display. None while the display also
    /// shows on another monitor: a cloned desktop has one mode for both.
    pub fn preferred_size(display_name: &str) -> Option<(u32, u32)> {
        let paths = select::windows::active_paths();
        let mut shown = paths
            .iter()
            .filter(|path| select::windows::source_name(path).as_deref() == Some(display_name));
        match (shown.next(), shown.next()) {
            (Some(path), None) if select::windows::is_kvm_target(path) => {
                select::windows::preferred_size(path)
            }
            _ => None,
        }
    }

    /// Sets the size and keeps the other properties of the mode. Stored in
    /// the registry: Windows then gives the monitor this mode when it sees
    /// it again.
    pub fn set_mode(display_name: &str, (width, height): (u32, u32)) -> Result<(), Box<dyn Error>> {
        let device: Vec<u16> = display_name.encode_utf16().chain([0]).collect();
        let mut mode: DEVMODEW = unsafe { mem::zeroed() };
        mode.dmSize = mem::size_of::<DEVMODEW>() as u16;
        mode.dmFields = DM_PELSWIDTH | DM_PELSHEIGHT;
        mode.dmPelsWidth = width;
        mode.dmPelsHeight = height;
        let status = unsafe {
            ChangeDisplaySettingsExW(
                device.as_ptr(),
                &mut mode,
                ptr::null_mut(),
                CDS_UPDATEREGISTRY,
                ptr::null_mut(),
            )
        };
        if status != DISP_CHANGE_SUCCESSFUL {
            return Err(format!("Windows could not set the display mode: error {status}").into());
        }
        Ok(())
    }

    #[link(name = "user32")]
    extern "system" {
        fn SetDisplayConfig(
            path_count: u32,
            paths: *mut c_void,
            mode_count: u32,
            modes: *mut c_void,
            flags: u32,
        ) -> i32;
    }

    pub fn apply_layout(_display_name: &str, layout: DisplayLayout) -> Result<(), Box<dyn Error>> {
        let active = Display::all()?
            .into_iter()
            .filter(Display::is_online)
            .count();
        // The desktop counts mirrored displays as one monitor.
        let desktop_monitors = unsafe { GetSystemMetrics(SM_CMONITORS) } as usize;
        let (needed, topology) = match layout {
            DisplayLayout::Mirror => (desktop_monitors != 1, SDC_TOPOLOGY_CLONE),
            DisplayLayout::Extend => (desktop_monitors < active, SDC_TOPOLOGY_EXTEND),
        };
        if active <= 1 || !needed {
            return Ok(());
        }
        let status = unsafe {
            SetDisplayConfig(0, ptr::null_mut(), 0, ptr::null_mut(), SDC_APPLY | topology)
        };
        if status != 0 {
            return Err(
                format!("Windows could not change display topology: error {status}").into(),
            );
        }
        Ok(())
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::platform::xrandr_output;

    const PROPERTIES: &str = "\
Screen 0: minimum 320 x 200
HDMI-1 connected primary 1920x1080+0+0
\tEDID: 00ffffffffffff00
\tCONNECTOR_ID: 95
DP-1 connected 2560x1440+1920+0
\tCONNECTOR_ID: 103
";

    #[test]
    fn finds_the_output_with_the_connector_id() {
        assert_eq!(xrandr_output(PROPERTIES, 95), Some("HDMI-1"));
        assert_eq!(xrandr_output(PROPERTIES, 103), Some("DP-1"));
        assert_eq!(xrandr_output(PROPERTIES, 7), None);
    }

    #[test]
    fn an_ambiguous_connector_id_names_no_output() {
        let twice = format!("{PROPERTIES}DP-2 connected\n\tCONNECTOR_ID: 95\n");
        assert_eq!(xrandr_output(&twice, 95), None);
    }
}
