//! Which display capture binds to: the one the user picked in the KVM's web
//! UI, or else the display whose EDID the KVM presents. A temporary display
//! the desktop creates during a hotplug is never picked automatically.

use super::{describe, find_online, Display};
use std::{collections::HashSet, error::Error};

pub struct DisplaySelector {
    /// The display the user picked; None for automatic selection.
    requested: Option<String>,
    selected: Option<String>,
    /// The displays that the log last showed with a selection.
    logged: String,
}

impl DisplaySelector {
    pub fn new(requested: Option<String>) -> Self {
        Self {
            requested,
            selected: None,
            logged: String::new(),
        }
    }

    /// Logs the displays that a selection chose from, when they differ from
    /// the last ones logged: capture on Windows selects the display again
    /// once a second while it tries DXGI.
    pub fn log_selection(&mut self, displays: String) {
        if displays != self.logged {
            eprintln!("capture display selected: {displays}");
            self.logged = displays;
        }
    }

    /// Follows a new pick from the web UI; true when it changed.
    pub fn update_requested(&mut self, requested: Option<String>) -> bool {
        if self.requested == requested {
            return false;
        }
        self.requested = requested;
        self.selected = None;
        true
    }

    /// The display to capture from `displays`, or None while it is absent.
    /// Several KVM displays and no pick is an error: the user must choose.
    pub fn select(&mut self, displays: Vec<Display>) -> Result<Option<Display>, Box<dyn Error>> {
        let identities: Vec<_> = displays
            .iter()
            .map(|display| (display.name(), display.is_online()))
            .collect();
        let index = select_index(
            self.requested.as_deref(),
            self.selected.as_deref(),
            &identities,
            &kvm_display_names(),
        )
        .map_err(|reason| {
            format!(
                "{reason}: {}; pick the capture display in the KVM web UI",
                describe(&displays)
            )
        })?;
        let display = index.and_then(|index| displays.into_iter().nth(index));
        if let Some(display) = &display {
            self.selected = Some(display.name());
        }
        Ok(display)
    }

    pub fn selected_name(&self) -> &str {
        self.selected
            .as_deref()
            .expect("a display has been selected")
    }

    /// The selected display, when it is known, enumerated and inactive: a
    /// display asleep. An unknown or missing display, or an enumeration
    /// error, says nothing about sleep.
    pub fn inactive_display(&self) -> Result<Option<String>, Box<dyn Error>> {
        let displays: Vec<_> = Display::all()?
            .into_iter()
            .map(|display| (display.name(), display.is_online()))
            .collect();
        Ok(inactive_name(
            self.requested.as_deref().or(self.selected.as_deref()),
            &displays,
            &kvm_display_names(),
        ))
    }

    /// The selected display's current size, or None while it is offline.
    pub fn selected_mode(&self) -> Result<Option<(usize, usize)>, Box<dyn Error>> {
        let Some(name) = &self.selected else {
            return Ok(None);
        };
        Ok(find_online(name)?.map(|display| (display.width(), display.height())))
    }
}

fn select_index(
    requested: Option<&str>,
    selected: Option<&str>,
    displays: &[(String, bool)],
    kvm_names: &HashSet<String>,
) -> Result<Option<usize>, &'static str> {
    if let Some(name) = requested {
        return Ok(displays
            .iter()
            .position(|(id, online)| *online && id == name));
    }
    let candidates: Vec<(usize, &str)> = displays
        .iter()
        .enumerate()
        .filter(|(_, (id, online))| *online && kvm_names.contains(id))
        .map(|(index, (id, _))| (index, id.as_str()))
        .collect();
    if let Some((index, _)) = candidates.iter().find(|(_, id)| Some(*id) == selected) {
        return Ok(Some(*index));
    }
    match candidates.as_slice() {
        [] => Ok(None),
        [(index, _)] => Ok(Some(*index)),
        _ => Err("multiple KVM displays"),
    }
}

fn inactive_name(
    preferred: Option<&str>,
    displays: &[(String, bool)],
    kvm_names: &HashSet<String>,
) -> Option<String> {
    if let Some(name) = preferred {
        return displays
            .iter()
            .find(|(id, online)| id == name && !online)
            .map(|(id, _)| id.clone());
    }
    let mut matches = displays.iter().filter(|(id, _)| kvm_names.contains(id));
    match (matches.next(), matches.next()) {
        (Some((name, false)), None) => Some(name.clone()),
        _ => None,
    }
}

/// The names of the displays whose EDID the KVM presents.
#[cfg(target_os = "macos")]
pub(super) fn kvm_display_names() -> HashSet<String> {
    use core_graphics::display::CGDisplay;
    // A macOS display is named by its CGDirectDisplayID.
    CGDisplay::active_displays()
        .unwrap_or_default()
        .into_iter()
        .filter(|id| CGDisplay::new(*id).vendor_number() == u32::from(super::JETKVM_EDID_VENDOR))
        .map(|id| id.to_string())
        .collect()
}

#[cfg(target_os = "linux")]
pub(super) fn kvm_display_names() -> HashSet<String> {
    let vendor = super::JETKVM_EDID_VENDOR.to_be_bytes();
    Display::all()
        .unwrap_or_default()
        .into_iter()
        .filter(|display| {
            std::fs::read(format!("/sys/class/drm/{}/edid", display.name()))
                .is_ok_and(|edid| edid.get(8..10) == Some(&vendor))
        })
        .map(|display| display.name())
        .collect()
}

#[cfg(windows)]
pub(super) fn kvm_display_names() -> HashSet<String> {
    windows::active_paths()
        .iter()
        .filter(|path| windows::is_kvm_target(path))
        .filter_map(windows::source_name)
        .collect()
}

#[cfg(windows)]
pub(super) mod windows {
    use crate::video::display::{windows::wide_string, JETKVM_EDID_VENDOR};
    use std::mem;
    use winapi::shared::minwindef::UINT;
    use winapi::um::wingdi::{
        DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME, DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME,
        DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_PREFERRED_MODE, DISPLAYCONFIG_DEVICE_INFO_HEADER,
        DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_PATH_INFO, DISPLAYCONFIG_SOURCE_DEVICE_NAME,
        DISPLAYCONFIG_TARGET_DEVICE_NAME, DISPLAYCONFIG_TARGET_PREFERRED_MODE,
        QDC_ONLY_ACTIVE_PATHS,
    };

    // The winapi crate declares the display configuration structs but not
    // these functions.
    #[link(name = "user32")]
    extern "system" {
        fn GetDisplayConfigBufferSizes(flags: u32, paths: *mut UINT, modes: *mut UINT) -> i32;
        fn QueryDisplayConfig(
            flags: u32,
            num_paths: *mut UINT,
            paths: *mut DISPLAYCONFIG_PATH_INFO,
            num_modes: *mut UINT,
            modes: *mut DISPLAYCONFIG_MODE_INFO,
            topology: *mut u32,
        ) -> i32;
        fn DisplayConfigGetDeviceInfo(header: *mut DISPLAYCONFIG_DEVICE_INFO_HEADER) -> i32;
    }

    /// The active display paths; empty when Windows will not say.
    pub fn active_paths() -> Vec<DISPLAYCONFIG_PATH_INFO> {
        let (mut num_paths, mut num_modes): (UINT, UINT) = (0, 0);
        let sized = unsafe {
            GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut num_paths, &mut num_modes)
        };
        if sized != 0 {
            return Vec::new();
        }
        let mut paths = vec![unsafe { mem::zeroed() }; num_paths as usize];
        let mut modes: Vec<DISPLAYCONFIG_MODE_INFO> =
            vec![unsafe { mem::zeroed() }; num_modes as usize];
        let queried = unsafe {
            QueryDisplayConfig(
                QDC_ONLY_ACTIVE_PATHS,
                &mut num_paths,
                paths.as_mut_ptr(),
                &mut num_modes,
                modes.as_mut_ptr(),
                std::ptr::null_mut(),
            )
        };
        if queried != 0 {
            return Vec::new();
        }
        paths.truncate(num_paths as usize);
        paths
    }

    /// True when the path's monitor has the KVM's EDID. Some systems report
    /// the manufacturer id byte-swapped; the friendly name covers those
    /// that report neither.
    pub fn is_kvm_target(path: &DISPLAYCONFIG_PATH_INFO) -> bool {
        let mut target: DISPLAYCONFIG_TARGET_DEVICE_NAME = unsafe { mem::zeroed() };
        target.header._type = DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME;
        target.header.size = mem::size_of::<DISPLAYCONFIG_TARGET_DEVICE_NAME>() as u32;
        target.header.adapterId = path.targetInfo.adapterId;
        target.header.id = path.targetInfo.id;
        if unsafe { DisplayConfigGetDeviceInfo(&mut target.header) } != 0 {
            return false;
        }
        let vendor = target.edidManufactureId;
        vendor == JETKVM_EDID_VENDOR
            || vendor == JETKVM_EDID_VENDOR.swap_bytes()
            || wide_string(&target.monitorFriendlyDeviceName)
                .to_ascii_uppercase()
                .starts_with("JETKVM")
    }

    /// The size of the mode that the EDID of the path's monitor prefers.
    pub fn preferred_size(path: &DISPLAYCONFIG_PATH_INFO) -> Option<(u32, u32)> {
        let mut mode: DISPLAYCONFIG_TARGET_PREFERRED_MODE = unsafe { mem::zeroed() };
        mode.header._type = DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_PREFERRED_MODE;
        mode.header.size = mem::size_of::<DISPLAYCONFIG_TARGET_PREFERRED_MODE>() as u32;
        mode.header.adapterId = path.targetInfo.adapterId;
        mode.header.id = path.targetInfo.id;
        (unsafe { DisplayConfigGetDeviceInfo(&mut mode.header) } == 0)
            .then_some((mode.width, mode.height))
    }

    /// The GDI device name of the path's source, which is the display's
    /// name.
    pub fn source_name(path: &DISPLAYCONFIG_PATH_INFO) -> Option<String> {
        let mut source: DISPLAYCONFIG_SOURCE_DEVICE_NAME = unsafe { mem::zeroed() };
        source.header._type = DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME;
        source.header.size = mem::size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32;
        source.header.adapterId = path.sourceInfo.adapterId;
        source.header.id = path.sourceInfo.id;
        (unsafe { DisplayConfigGetDeviceInfo(&mut source.header) } == 0)
            .then(|| wide_string(&source.viewGdiDeviceName))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(ids: &[&str]) -> HashSet<String> {
        ids.iter().map(|id| id.to_string()).collect()
    }

    #[test]
    fn automatic_capture_never_adopts_a_temporary_headless_display() {
        let displays = vec![("2".into(), false), ("492".into(), true)];
        let kvm = names(&["2"]);
        assert_eq!(select_index(None, Some("2"), &displays, &kvm), Ok(None));
        assert_eq!(select_index(None, None, &displays, &kvm), Ok(None));
        assert_eq!(
            select_index(None, Some("2"), &displays[1..], &kvm),
            Ok(None)
        );
    }

    #[test]
    fn automatic_capture_follows_the_kvm_display_after_its_id_changes() {
        let displays = vec![("492".into(), true), ("new-kvm".into(), true)];
        let kvm = names(&["new-kvm"]);
        assert_eq!(select_index(None, Some("2"), &displays, &kvm), Ok(Some(1)));
        assert_eq!(
            select_index(None, Some("492"), &displays, &kvm),
            Ok(Some(1))
        );
    }

    #[test]
    fn several_kvm_displays_keep_a_known_selection_but_are_not_guessed() {
        let displays = vec![("one".into(), true), ("two".into(), true)];
        let kvm = names(&["one", "two"]);
        assert_eq!(
            select_index(None, Some("two"), &displays, &kvm),
            Ok(Some(1))
        );
        assert!(select_index(None, None, &displays, &kvm).is_err());
        assert!(select_index(None, Some("gone"), &displays, &kvm).is_err());
    }

    #[test]
    fn a_pick_can_capture_any_display_and_waits_for_it() {
        let displays = vec![("other".into(), true), ("kvm".into(), true)];
        let kvm = names(&["kvm"]);
        assert_eq!(
            select_index(Some("other"), None, &displays, &kvm),
            Ok(Some(0))
        );
        assert_eq!(
            select_index(Some("other"), None, &displays[1..], &kvm),
            Ok(None)
        );
    }

    #[test]
    fn only_a_known_inactive_display_is_asleep() {
        let displays = vec![("kvm".into(), false), ("other".into(), true)];
        let kvm = names(&["kvm"]);
        assert_eq!(
            inactive_name(Some("kvm"), &displays, &kvm),
            Some("kvm".into())
        );
        assert_eq!(inactive_name(None, &displays, &kvm), Some("kvm".into()));
        assert_eq!(inactive_name(Some("other"), &displays, &kvm), None);
        assert_eq!(inactive_name(Some("missing"), &displays, &kvm), None);
        assert_eq!(inactive_name(None, &displays, &HashSet::new()), None);
        assert_eq!(
            inactive_name(None, &displays, &names(&["kvm", "other"])),
            None
        );
    }
}
