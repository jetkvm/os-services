//! A ScreenCaptureKit stream of one display (`screen_capture.m`): NV12 frames
//! at the display's pixel size with the pointer drawn in.

use crate::video::nv12::Nv12Layout;
use std::{
    ffi::{c_char, c_void, CStr},
    io,
    time::Duration,
};

extern "C" {
    fn guest_capture_start(
        display_id: u32,
        width: usize,
        height: usize,
        fps: i32,
        error: *mut c_char,
        error_length: usize,
    ) -> *mut c_void;
    fn guest_capture_next(
        handle: *mut c_void,
        timeout_ms: i32,
        stopped: *mut i32,
        error: *mut c_char,
        error_length: usize,
    ) -> *mut c_void;
    fn guest_capture_copy(
        frame: *mut c_void,
        dst: *mut u8,
        dst_length: usize,
        width: usize,
        height: usize,
        y_stride: usize,
        uv_offset: usize,
        uv_stride: usize,
    ) -> i32;
    fn guest_capture_release_frame(frame: *mut c_void);
    fn guest_capture_stop(handle: *mut c_void);
}

pub struct ScreenCapture {
    handle: *mut c_void,
}

// The stream is used from one thread at a time; ScreenCaptureKit delivers
// frames on its own queue.
unsafe impl Send for ScreenCapture {}

impl ScreenCapture {
    pub fn start(display_id: u32, width: usize, height: usize, fps: u64) -> io::Result<Self> {
        let mut error = [0u8; 256];
        let handle = unsafe {
            guest_capture_start(
                display_id,
                width,
                height,
                fps as i32,
                error.as_mut_ptr().cast(),
                error.len(),
            )
        };
        if handle.is_null() {
            return Err(io::Error::other(message(&error)));
        }
        Ok(Self { handle })
    }

    /// The newest frame, waiting up to `timeout` for one.
    pub fn next(&mut self, timeout: Duration) -> Next {
        let mut error = [0u8; 256];
        let mut stopped = 0;
        let frame = unsafe {
            guest_capture_next(
                self.handle,
                timeout.as_millis() as i32,
                &mut stopped,
                error.as_mut_ptr().cast(),
                error.len(),
            )
        };
        match (frame.is_null(), stopped) {
            (false, _) => Next::Frame(Frame(frame)),
            (true, 0) => Next::Idle,
            (true, _) => Next::Stopped(message(&error)),
        }
    }
}

/// What a wait for the next frame got.
pub enum Next {
    Frame(Frame),
    /// No new frame arrived.
    Idle,
    /// The stream ended, with the reason ScreenCaptureKit gave, for example
    /// when the display went away.
    Stopped(String),
}

/// A captured frame: a retained pixel buffer that ScreenCaptureKit no longer
/// writes.
pub struct Frame(*mut c_void);

// A pixel buffer's retain count is thread-safe, and the frame is only read.
unsafe impl Send for Frame {}

impl Frame {
    /// Writes the frame into `nv12` with `layout`.
    pub fn copy_into(&self, nv12: &mut Vec<u8>, layout: &Nv12Layout) -> io::Result<()> {
        nv12.resize(layout.buffer_len(), 0);
        let copied = unsafe {
            guest_capture_copy(
                self.0,
                nv12.as_mut_ptr(),
                nv12.len(),
                layout.width,
                layout.height,
                layout.y_stride,
                layout.uv_offset,
                layout.uv_stride,
            )
        };
        if copied != 0 {
            return Err(io::Error::other(format!(
                "the captured frame is not {}x{} NV12",
                layout.width, layout.height
            )));
        }
        Ok(())
    }
}

impl Drop for Frame {
    fn drop(&mut self) {
        unsafe { guest_capture_release_frame(self.0) };
    }
}

impl Drop for ScreenCapture {
    fn drop(&mut self) {
        unsafe { guest_capture_stop(self.handle) };
    }
}

fn message(error: &[u8]) -> String {
    CStr::from_bytes_until_nul(error)
        .map(|text| text.to_string_lossy().into_owned())
        .unwrap_or_default()
}
