//! DRM displays, enumerated through the native KMS capture bridge.

use std::{
    ffi::{CStr, CString},
    io,
    os::raw::c_char,
};

/// `drmtap_display` in the native bridge.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct RawDisplay {
    crtc: u32,
    connector: u32,
    name: [c_char; 32],
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    refresh_hz: u32,
    active: i32,
}

extern "C" {
    fn kms_list_displays(device: *const c_char, out: *mut RawDisplay, count: i32) -> i32;
}

pub struct Display {
    card: String,
    output: String,
    raw: RawDisplay,
}

impl Display {
    pub fn all() -> io::Result<Vec<Self>> {
        let mut displays = Vec::new();
        for entry in std::fs::read_dir("/sys/class/drm")? {
            let name = entry?.file_name().to_string_lossy().into_owned();
            if !name
                .strip_prefix("card")
                .is_some_and(|n| !n.is_empty() && n.bytes().all(|c| c.is_ascii_digit()))
            {
                continue;
            }
            let device = CString::new(format!("/dev/dri/{name}")).unwrap();
            let mut outputs = [RawDisplay::default(); 64];
            let count = unsafe {
                kms_list_displays(device.as_ptr(), outputs.as_mut_ptr(), outputs.len() as i32)
            };
            for raw in outputs.into_iter().take(count.max(0) as usize) {
                // The bridge terminates every name; skip one that is not.
                let bytes = raw.name.map(|c| c as u8);
                let Ok(output) = CStr::from_bytes_until_nul(&bytes) else {
                    continue;
                };
                displays.push(Self {
                    card: name.clone(),
                    output: output.to_string_lossy().into_owned(),
                    raw,
                });
            }
        }
        displays.sort_by_key(Self::name);
        Ok(displays)
    }

    pub fn name(&self) -> String {
        format!("{}-{}", self.card, self.output)
    }
    pub fn width(&self) -> usize {
        self.raw.width as usize
    }
    pub fn height(&self) -> usize {
        self.raw.height as usize
    }
    pub fn origin(&self) -> (i32, i32) {
        (self.raw.x as i32, self.raw.y as i32)
    }
    pub fn is_online(&self) -> bool {
        self.raw.active != 0 && self.raw.width != 0 && self.raw.height != 0
    }
    pub fn is_primary(&self) -> bool {
        self.origin() == (0, 0)
    }
    pub fn device(&self) -> String {
        format!("/dev/dri/{}", self.card)
    }
    pub fn connector(&self) -> u32 {
        self.raw.connector
    }
}
