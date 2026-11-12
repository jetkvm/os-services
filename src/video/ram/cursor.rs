//! The pointer overlay for Windows captures. The duplication API delivers
//! frames without the hardware pointer, so the pointer is drawn with GDI into
//! a small BGRA surface over the pixels beneath it, written into the NV12
//! frame, and removed again once the frame is handed on.

use crate::video::nv12::{
    pixels::{self, Rect, SavedRegion, BGRA_BYTES},
    Nv12Layout,
};
use std::{
    io,
    mem::{self, size_of},
    ptr, slice,
};
use winapi::{
    shared::windef::{HBITMAP, HCURSOR, HDC, HGDIOBJ},
    um::{
        wingdi::{
            CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetObjectW, SelectObject,
            BITMAP, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS,
        },
        winuser::{DrawIconEx, GetCursorInfo, GetIconInfo, CURSORINFO, CURSOR_SHOWING, ICONINFO},
    },
};

const DI_NORMAL: u32 = 0x0003;

/// The pointer as the next frame shows it, in frame coordinates.
#[derive(Clone, Copy, Eq, PartialEq)]
struct CursorState {
    handle: HCURSOR,
    x: i32,
    y: i32,
    shape: Option<CursorShape>,
}

#[derive(Clone, Copy, Eq, PartialEq)]
struct CursorShape {
    handle: HCURSOR,
    hotspot_x: i32,
    hotspot_y: i32,
    width: i32,
    height: i32,
}

impl CursorShape {
    /// Keeps the pointer at least as large relative to the video as a 32px
    /// pointer at 1080p, so it stays visible on a high-resolution desktop
    /// at 100% scaling. A pointer the user enlarged is not shrunk.
    fn for_frame_height(mut self, frame_height: usize) -> Self {
        let minimum = (frame_height as i64 * 32 / 1080).max(32);
        if self.height > 0 && i64::from(self.height) < minimum {
            let height = i64::from(self.height);
            let scaled = |value: i32| ((i64::from(value) * minimum + height / 2) / height) as i32;
            self.width = scaled(self.width);
            self.hotspot_x = scaled(self.hotspot_x);
            self.hotspot_y = scaled(self.hotspot_y);
            self.height = minimum as i32;
        }
        self
    }
}

/// A memory DC with a top-down 32-bit BGRA bitmap selected into it: the
/// surface the pointer is drawn on, and the one GDI capture copies into.
pub(super) struct Surface {
    dc: HDC,
    bitmap: HBITMAP,
    old_bitmap: HGDIOBJ,
    bits: *mut u8,
    width: usize,
    height: usize,
}

impl Surface {
    pub(super) fn new() -> io::Result<Self> {
        let dc = unsafe { CreateCompatibleDC(ptr::null_mut()) };
        if dc.is_null() {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            dc,
            bitmap: ptr::null_mut(),
            old_bitmap: ptr::null_mut(),
            bits: ptr::null_mut(),
            width: 0,
            height: 0,
        })
    }

    /// A top-down 32-bit bitmap of this size, selected into the DC.
    pub(super) fn resize(&mut self, width: usize, height: usize) -> io::Result<()> {
        if (self.width, self.height) == (width, height) && !self.bitmap.is_null() {
            return Ok(());
        }
        self.release();
        let (bitmap, bits) = top_down_bitmap(self.dc, width, height)?;
        let old_bitmap = unsafe { SelectObject(self.dc, bitmap as HGDIOBJ) };
        if old_bitmap.is_null() {
            unsafe { DeleteObject(bitmap as HGDIOBJ) };
            return Err(io::Error::last_os_error());
        }
        (self.bitmap, self.old_bitmap, self.bits) = (bitmap, old_bitmap, bits);
        (self.width, self.height) = (width, height);
        Ok(())
    }

    pub(super) fn dc(&self) -> HDC {
        self.dc
    }

    pub(super) fn pixels(&mut self) -> &mut [u8] {
        unsafe { slice::from_raw_parts_mut(self.bits, self.width * self.height * BGRA_BYTES) }
    }

    fn draw_icon(&self, x: i32, y: i32, shape: CursorShape) -> io::Result<()> {
        let drawn = unsafe {
            DrawIconEx(
                self.dc,
                x,
                y,
                shape.handle,
                shape.width,
                shape.height,
                0,
                ptr::null_mut(),
                DI_NORMAL,
            )
        };
        if drawn == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn release(&mut self) {
        if self.bitmap.is_null() {
            return;
        }
        unsafe {
            SelectObject(self.dc, self.old_bitmap);
            DeleteObject(self.bitmap as HGDIOBJ);
        }
        self.bitmap = ptr::null_mut();
        self.old_bitmap = ptr::null_mut();
        self.bits = ptr::null_mut();
    }
}

impl Drop for Surface {
    fn drop(&mut self) {
        self.release();
        unsafe { DeleteDC(self.dc) };
    }
}

pub struct CursorCompositor {
    surface: Surface,
    /// A fresh frame has been seen since the last reset, so an idle pass has
    /// a background to draw on.
    has_background: bool,
    last_state: Option<CursorState>,
    shape: Option<CursorShape>,
    saved: SavedRegion,
    /// A draw failure is logged once per run of failures.
    draw_error_logged: bool,
}

impl CursorCompositor {
    pub fn new() -> io::Result<Self> {
        Ok(Self {
            surface: Surface::new()?,
            has_background: false,
            last_state: None,
            shape: None,
            saved: SavedRegion::default(),
            draw_error_logged: false,
        })
    }

    /// Draws the pointer into `frame`, hands the frame to `callback`, and
    /// restores it. An idle pass, one that is not `fresh`, draws only when
    /// the pointer changed and returns None otherwise.
    pub fn draw<R>(
        &mut self,
        fresh: bool,
        layout: Nv12Layout,
        origin: (i32, i32),
        frame: &mut [u8],
        callback: impl FnOnce(&[u8]) -> R,
    ) -> io::Result<Option<R>> {
        if fresh {
            self.has_background = true;
        } else if !self.has_background {
            return Ok(None);
        }
        let state = self.cursor_state(origin, layout.height)?;
        if !fresh && self.last_state == Some(state) {
            return Ok(None);
        }
        self.last_state = Some(state);
        // Pointer handles owned by other processes can become invalid between
        // the state read and the draw. A failed draw skips the pointer for
        // this frame and never fails the capture.
        match self.overlay(state, &layout, frame) {
            Ok(()) => self.draw_error_logged = false,
            Err(error) => {
                self.last_state = None;
                if !self.draw_error_logged {
                    self.draw_error_logged = true;
                    eprintln!("cursor overlay skipped: {error}");
                }
                if !fresh {
                    return Ok(None);
                }
            }
        }
        let result = callback(frame);
        self.saved.restore(frame, &layout);
        Ok(Some(result))
    }

    /// The next idle pass redraws the pointer even if it has not moved.
    pub fn invalidate(&mut self) {
        self.last_state = None;
    }

    pub fn reset(&mut self) {
        self.has_background = false;
        self.last_state = None;
        self.shape = None;
        self.saved.clear();
    }

    /// Draws the pointer over the pixels beneath it and writes the result
    /// into the frame, keeping what it covers.
    fn overlay(
        &mut self,
        state: CursorState,
        layout: &Nv12Layout,
        frame: &mut [u8],
    ) -> io::Result<()> {
        self.saved.clear();
        let Some(shape) = state.shape else {
            return Ok(());
        };
        let (x, y) = (state.x - shape.hotspot_x, state.y - shape.hotspot_y);
        let Some(rect) = Rect::clipped(x, y, shape.width, shape.height, layout) else {
            return Ok(());
        };
        self.surface.resize(rect.width, rect.height)?;
        pixels::nv12_to_bgra(frame, layout, rect, self.surface.pixels());
        self.surface
            .draw_icon(x - rect.left as i32, y - rect.top as i32, shape)?;
        self.saved.save(frame, layout, rect);
        let stride = rect.width * BGRA_BYTES;
        pixels::bgra_to_nv12(self.surface.pixels(), stride, frame, layout, rect);
        Ok(())
    }

    fn cursor_state(&mut self, origin: (i32, i32), frame_height: usize) -> io::Result<CursorState> {
        let mut info: CURSORINFO = unsafe { mem::zeroed() };
        info.cbSize = size_of::<CURSORINFO>() as _;
        // The secure desktop hides the pointer from this session.
        if unsafe { GetCursorInfo(&mut info) } == 0 {
            return Ok(CursorState {
                handle: ptr::null_mut(),
                x: 0,
                y: 0,
                shape: None,
            });
        }
        // A hidden pointer keeps its handle and position in the state, so an
        // idle pass still sends a frame when it moves. A fullscreen game's
        // hardware pointer cannot be read from another process; the game
        // draws that pointer into the frame itself.
        let showing = info.flags & CURSOR_SHOWING != 0 && !info.hCursor.is_null();
        let shape = showing
            .then(|| self.cursor_shape(info.hCursor).ok())
            .flatten()
            .map(|shape| shape.for_frame_height(frame_height));
        Ok(CursorState {
            handle: info.hCursor,
            x: info.ptScreenPos.x - origin.0,
            y: info.ptScreenPos.y - origin.1,
            shape: shape.filter(|shape| shape.width > 0 && shape.height > 0),
        })
    }

    fn cursor_shape(&mut self, handle: HCURSOR) -> io::Result<CursorShape> {
        if let Some(shape) = self.shape.filter(|shape| shape.handle == handle) {
            return Ok(shape);
        }
        let mut info: ICONINFO = unsafe { mem::zeroed() };
        if unsafe { GetIconInfo(handle, &mut info) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // A monochrome pointer stacks its AND and XOR masks in one bitmap of
        // twice the height.
        let dimensions = if info.hbmColor.is_null() {
            bitmap_dimensions(info.hbmMask).map(|(width, height)| (width, height / 2))
        } else {
            bitmap_dimensions(info.hbmColor)
        };
        for bitmap in [info.hbmColor, info.hbmMask] {
            if !bitmap.is_null() {
                unsafe { DeleteObject(bitmap as HGDIOBJ) };
            }
        }
        let (width, height) = dimensions?;
        let shape = CursorShape {
            handle,
            hotspot_x: info.xHotspot as i32,
            hotspot_y: info.yHotspot as i32,
            width,
            height,
        };
        self.shape = Some(shape);
        Ok(shape)
    }
}

/// A top-down 32-bit BGRA DIB section for `dc`, and its pixels.
fn top_down_bitmap(dc: HDC, width: usize, height: usize) -> io::Result<(HBITMAP, *mut u8)> {
    let mut info: BITMAPINFO = unsafe { mem::zeroed() };
    info.bmiHeader = BITMAPINFOHEADER {
        biSize: size_of::<BITMAPINFOHEADER>() as _,
        biWidth: width as i32,
        biHeight: -(height as i32),
        biPlanes: 1,
        biBitCount: (BGRA_BYTES * 8) as _,
        biCompression: BI_RGB,
        ..unsafe { mem::zeroed() }
    };
    let mut bits = ptr::null_mut();
    let bitmap =
        unsafe { CreateDIBSection(dc, &info, DIB_RGB_COLORS, &mut bits, ptr::null_mut(), 0) };
    if bitmap.is_null() || bits.is_null() {
        return Err(io::Error::last_os_error());
    }
    Ok((bitmap, bits.cast()))
}

fn bitmap_dimensions(bitmap: HBITMAP) -> io::Result<(i32, i32)> {
    let mut info: BITMAP = unsafe { mem::zeroed() };
    let size = size_of::<BITMAP>() as i32;
    if unsafe { GetObjectW(bitmap as HGDIOBJ, size, (&mut info as *mut BITMAP).cast()) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((info.bmWidth.abs(), info.bmHeight.abs()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shape(size: i32) -> CursorShape {
        CursorShape {
            handle: ptr::null_mut(),
            hotspot_x: 4,
            hotspot_y: 8,
            width: size,
            height: size,
        }
    }

    #[test]
    fn pointer_scales_with_video_and_keeps_its_hotspot() {
        let cursor = shape(32).for_frame_height(2160);
        assert_eq!((cursor.width, cursor.height), (64, 64));
        assert_eq!((cursor.hotspot_x, cursor.hotspot_y), (8, 16));
    }

    #[test]
    fn pointer_keeps_native_and_accessibility_sizes() {
        for (size, frame_height) in [(32, 720), (32, 1080), (96, 2160)] {
            let cursor = shape(size).for_frame_height(frame_height);
            assert_eq!((cursor.width, cursor.height), (size, size));
            assert_eq!((cursor.hotspot_x, cursor.hotspot_y), (4, 8));
        }
    }
}
