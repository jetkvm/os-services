//! Windows displays: the DXGI outputs attached to the desktop, named by their
//! GDI device name (`\\.\DISPLAYn`). Without DXGI, GDI lists them.

use std::{io, mem, ptr};
use winapi::{
    shared::{
        dxgi::{CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1, IDXGIOutput, IID_IDXGIFactory1},
        dxgi1_2::IDXGIOutput1,
        dxgitype::DXGI_MODE_ROTATION,
        minwindef::FALSE,
        winerror::S_OK,
    },
    um::{
        unknwnbase::IUnknown,
        wingdi::{
            DEVMODEW, DISPLAY_DEVICEW, DISPLAY_DEVICE_ACTIVE, DISPLAY_DEVICE_MIRRORING_DRIVER,
        },
        winuser::{EnumDisplayDevicesW, EnumDisplaySettingsExW, ENUM_CURRENT_SETTINGS},
    },
    Interface,
};

/// An owned COM interface pointer.
pub struct Com<T: Interface>(*mut T);

impl<T: Interface> Com<T> {
    /// Takes ownership of a pointer a COM call returned; None for null.
    pub fn new(raw: *mut T) -> Option<Self> {
        (!raw.is_null()).then(|| Self(raw))
    }

    pub fn as_ptr(&self) -> *mut T {
        self.0
    }

    /// Asks the object for another of its interfaces.
    pub fn cast<U: Interface>(&self) -> Option<Com<U>> {
        let mut other = ptr::null_mut();
        let result =
            unsafe { (*(self.0 as *mut IUnknown)).QueryInterface(&U::uuidof(), &mut other) };
        if result == S_OK {
            Com::new(other as *mut U)
        } else {
            None
        }
    }
}

impl<T: Interface> std::ops::Deref for Com<T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { &*self.0 }
    }
}

impl<T: Interface> Clone for Com<T> {
    fn clone(&self) -> Self {
        unsafe { (*(self.0 as *mut IUnknown)).AddRef() };
        Self(self.0)
    }
}

impl<T: Interface> Drop for Com<T> {
    fn drop(&mut self) {
        unsafe { (*(self.0 as *mut IUnknown)).Release() };
    }
}

/// The DXGI objects that duplicate a display.
pub struct Output {
    pub adapter: Com<IDXGIAdapter1>,
    pub output: Com<IDXGIOutput1>,
    pub rotation: DXGI_MODE_ROTATION,
}

pub struct Display {
    name: String,
    origin: (i32, i32),
    width: usize,
    height: usize,
    online: bool,
    /// None when GDI listed the display.
    pub dxgi: Option<Output>,
}

impl Display {
    /// The displays in GDI's order. DXGI provides the duplication of each;
    /// when DXGI lists other displays than GDI, its list wins.
    pub fn all() -> io::Result<Vec<Self>> {
        let gdi = gdi_displays();
        let mut dxgi = dxgi_displays().unwrap_or_default();
        if dxgi.is_empty() {
            return Ok(gdi);
        }
        let position = |display: &Display| {
            gdi.iter()
                .position(|other| other.name == display.name)
                .unwrap_or(usize::MAX)
        };
        if dxgi.len() == gdi.len() {
            dxgi.sort_by_key(position);
        }
        Ok(dxgi)
    }

    pub fn name(&self) -> String {
        self.name.clone()
    }

    /// The GDI device name with its terminating NUL, for GDI calls.
    pub fn wide_name(&self) -> Vec<u16> {
        self.name.encode_utf16().chain([0]).collect()
    }

    pub fn width(&self) -> usize {
        self.width
    }

    pub fn height(&self) -> usize {
        self.height
    }

    pub fn origin(&self) -> (i32, i32) {
        self.origin
    }

    /// Whether the display is attached to the desktop. GDI lists only
    /// attached displays.
    pub fn is_online(&self) -> bool {
        self.online
    }

    pub fn is_primary(&self) -> bool {
        self.origin == (0, 0)
    }
}

fn factory() -> io::Result<Com<IDXGIFactory1>> {
    let mut factory = ptr::null_mut();
    let result = unsafe { CreateDXGIFactory1(&IID_IDXGIFactory1, &mut factory) };
    Com::new(factory as *mut IDXGIFactory1)
        .filter(|_| result == S_OK)
        .ok_or_else(|| io::Error::other(format!("CreateDXGIFactory1 failed: {result:#x}")))
}

fn dxgi_displays() -> io::Result<Vec<Display>> {
    let factory = factory()?;
    let mut displays = Vec::new();
    for adapter_index in 0.. {
        let mut adapter = ptr::null_mut();
        unsafe { factory.EnumAdapters1(adapter_index, &mut adapter) };
        let Some(adapter) = Com::new(adapter) else {
            break;
        };
        for output_index in 0.. {
            let mut output: *mut IDXGIOutput = ptr::null_mut();
            unsafe { adapter.EnumOutputs(output_index, &mut output) };
            let Some(output) = Com::new(output) else {
                break;
            };
            let mut desc = unsafe { mem::zeroed() };
            unsafe { output.GetDesc(&mut desc) };
            let Some(output) = output.cast::<IDXGIOutput1>() else {
                continue;
            };
            let bounds = desc.DesktopCoordinates;
            displays.push(Display {
                name: wide_string(&desc.DeviceName),
                origin: (bounds.left, bounds.top),
                width: (bounds.right - bounds.left) as usize,
                height: (bounds.bottom - bounds.top) as usize,
                online: desc.AttachedToDesktop != FALSE,
                dxgi: Some(Output {
                    adapter: adapter.clone(),
                    output,
                    rotation: desc.Rotation,
                }),
            });
        }
    }
    Ok(displays)
}

fn gdi_displays() -> Vec<Display> {
    let mut displays = Vec::new();
    for index in 0.. {
        let mut device: DISPLAY_DEVICEW = unsafe { mem::zeroed() };
        device.cb = mem::size_of::<DISPLAY_DEVICEW>() as u32;
        if unsafe { EnumDisplayDevicesW(ptr::null(), index, &mut device, 0) } == FALSE {
            break;
        }
        if device.StateFlags & DISPLAY_DEVICE_ACTIVE == 0
            || device.StateFlags & DISPLAY_DEVICE_MIRRORING_DRIVER != 0
        {
            continue;
        }
        let mut mode: DEVMODEW = unsafe { mem::zeroed() };
        mode.dmSize = mem::size_of::<DEVMODEW>() as u16;
        let found = unsafe {
            EnumDisplaySettingsExW(
                device.DeviceName.as_ptr(),
                ENUM_CURRENT_SETTINGS,
                &mut mode,
                0,
            )
        };
        if found == FALSE {
            continue;
        }
        let position = unsafe { mode.u1.s2().dmPosition };
        displays.push(Display {
            name: wide_string(&device.DeviceName),
            origin: (position.x, position.y),
            width: mode.dmPelsWidth as usize,
            height: mode.dmPelsHeight as usize,
            online: true,
            dxgi: None,
        });
    }
    displays
}

/// The text of a NUL-terminated UTF-16 buffer that Windows filled.
pub(super) fn wide_string(value: &[u16]) -> String {
    let end = value.iter().position(|&c| c == 0).unwrap_or(value.len());
    String::from_utf16_lossy(&value[..end])
}
