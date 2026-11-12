//! NV12 frames: a full-resolution Y plane followed by an interleaved U/V
//! plane at half resolution, in the layout an encoder reads.

#[cfg(any(windows, test))]
pub mod pixels;

/// Where the planes of an NV12 frame are.
#[derive(Clone, Copy, Debug)]
pub struct Nv12Layout {
    pub width: usize,
    pub height: usize,
    pub y_stride: usize,
    pub uv_offset: usize,
    pub uv_stride: usize,
}

impl Nv12Layout {
    /// The size of a frame buffer in this layout.
    pub fn buffer_len(&self) -> usize {
        self.uv_offset + self.uv_stride * self.height.div_ceil(2)
    }
}
