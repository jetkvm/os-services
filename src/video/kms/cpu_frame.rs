//! Tests of the CPU path's frame (native/cpu_frame.h): the conversion to
//! I420, the cursor drawn into it, and what counts as a change.

use std::os::raw::c_void;

extern "C" {
    fn kms_cpu_frame_new(width: i32, height: i32, red_first: i32) -> *mut c_void;
    fn kms_cpu_frame_update(
        handle: *mut c_void,
        pixels: *const u8,
        stride: u32,
        cursor: *const u32,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    ) -> i32;
    fn kms_cpu_frame_planes(
        handle: *mut c_void,
        y: *mut *const u8,
        u: *mut *const u8,
        v: *mut *const u8,
    );
    fn kms_cpu_frame_destroy(handle: *mut c_void);
}

const UNCHANGED: i32 = 0;
const PICTURE: i32 = 1;
const POINTER: i32 = 2;

/// A cursor: premultiplied ARGB8888 pixels, `width` wide, at (x, y).
struct Cursor<'a> {
    pixels: &'a [u32],
    width: i32,
    x: i32,
    y: i32,
}

struct Frame {
    handle: *mut c_void,
    width: usize,
    height: usize,
    red_first: bool,
}

impl Frame {
    fn new(width: usize, height: usize, red_first: bool) -> Self {
        let handle = unsafe { kms_cpu_frame_new(width as i32, height as i32, red_first as i32) };
        Self {
            handle,
            width,
            height,
            red_first,
        }
    }

    /// A screen of one color, in the frame's byte order, with a padded
    /// stride as scanouts have.
    fn screen(&self, (r, g, b): (u8, u8, u8)) -> Screen {
        let stride = self.width * 4 + 16;
        let pixel = if self.red_first {
            [r, g, b, 0xFF]
        } else {
            [b, g, r, 0xFF]
        };
        let mut bytes = vec![0; stride * self.height];
        for row in bytes.chunks_mut(stride) {
            for column in row[..self.width * 4].chunks_mut(4) {
                column.copy_from_slice(&pixel);
            }
        }
        Screen {
            bytes,
            stride,
            red_first: self.red_first,
        }
    }

    fn update(&mut self, screen: &Screen, cursor: Option<&Cursor>) -> i32 {
        let (pixels, x, y, width, height) = match cursor {
            Some(cursor) => (
                cursor.pixels.as_ptr(),
                cursor.x,
                cursor.y,
                cursor.width,
                cursor.pixels.len() as i32 / cursor.width,
            ),
            None => (std::ptr::null(), 0, 0, 0, 0),
        };
        unsafe {
            kms_cpu_frame_update(
                self.handle,
                screen.bytes.as_ptr(),
                screen.stride as u32,
                pixels,
                x,
                y,
                width,
                height,
            )
        }
    }

    fn luma(&self, x: usize, y: usize) -> u8 {
        self.planes().0[y * self.width + x]
    }

    /// The U and V sample that covers pixel (x, y).
    fn chroma(&self, x: usize, y: usize) -> (u8, u8) {
        let (_, u, v) = self.planes();
        let index = (y / 2) * self.width.div_ceil(2) + x / 2;
        (u[index], v[index])
    }

    fn planes(&self) -> (&[u8], &[u8], &[u8]) {
        let (mut y, mut u, mut v) = (std::ptr::null(), std::ptr::null(), std::ptr::null());
        unsafe { kms_cpu_frame_planes(self.handle, &mut y, &mut u, &mut v) };
        let chroma = self.width.div_ceil(2) * self.height.div_ceil(2);
        unsafe {
            (
                std::slice::from_raw_parts(y, self.width * self.height),
                std::slice::from_raw_parts(u, chroma),
                std::slice::from_raw_parts(v, chroma),
            )
        }
    }
}

impl Drop for Frame {
    fn drop(&mut self) {
        unsafe { kms_cpu_frame_destroy(self.handle) }
    }
}

struct Screen {
    bytes: Vec<u8>,
    stride: usize,
    red_first: bool,
}

impl Screen {
    fn set(&mut self, x: usize, y: usize, (r, g, b): (u8, u8, u8)) {
        let at = y * self.stride + x * 4;
        let pixel = if self.red_first { [r, g, b] } else { [b, g, r] };
        self.bytes[at..at + 3].copy_from_slice(&pixel);
    }
}

/// Y, U and V of an RGB color by the BT.601 equations at limited range,
/// in floating point.
fn bt601(r: f64, g: f64, b: f64) -> (f64, f64, f64) {
    let (r, g, b) = (r / 255.0, g / 255.0, b / 255.0);
    let y = 0.299 * r + 0.587 * g + 0.114 * b;
    (
        16.0 + 219.0 * y,
        128.0 + 224.0 * (b - y) / 1.772,
        128.0 + 224.0 * (r - y) / 1.402,
    )
}

fn assert_near(found: u8, wanted: f64, what: &str) {
    assert!(
        (f64::from(found) - wanted).abs() <= 1.0,
        "{what}: {found}, BT.601 gives {wanted:.2}"
    );
}

const BLACK: (u8, u8, u8) = (0, 0, 0);
const WHITE: (u8, u8, u8) = (255, 255, 255);
const OPAQUE_WHITE: u32 = 0xFFFF_FFFF;

/// The CPU path must give the colors that the VAAPI conversion gives, which
/// is BT.601 at limited range, in both byte orders libdrmtap returns.
#[test]
fn colors_convert_by_bt601_at_limited_range() {
    let colors = [
        BLACK,
        WHITE,
        (255, 0, 0),
        (0, 255, 0),
        (0, 0, 255),
        (128, 128, 128),
        (12, 200, 99),
    ];
    for red_first in [false, true] {
        for color in colors {
            let mut frame = Frame::new(6, 4, red_first);
            frame.update(&frame.screen(color), None);
            let (y, u, v) = bt601(color.0.into(), color.1.into(), color.2.into());
            let what = format!("{color:?}, red first: {red_first}");
            assert_near(frame.luma(5, 3), y, &format!("Y of {what}"));
            let (cb, cr) = frame.chroma(5, 3);
            assert_near(cb, u, &format!("U of {what}"));
            assert_near(cr, v, &format!("V of {what}"));
        }
    }
}

/// A chroma sample covers a 2x2 block; a sharp edge inside it averages.
#[test]
fn a_chroma_sample_is_the_mean_of_its_block() {
    let mut frame = Frame::new(4, 4, false);
    let mut screen = frame.screen(BLACK);
    screen.set(2, 0, (255, 0, 0));
    screen.set(3, 1, (255, 0, 0));
    frame.update(&screen, None);
    let (_, u, v) = bt601(127.5, 0.0, 0.0);
    let (cb, cr) = frame.chroma(2, 0);
    assert_near(cb, u, "U");
    assert_near(cr, v, "V");
    assert_eq!(frame.chroma(0, 0), (128, 128));
}

/// A compositor can draw into the framebuffer it shows, so a change is
/// found in the pixels; an unchanged screen encodes nothing.
#[test]
fn a_change_in_place_is_found_and_an_unchanged_screen_is_not() {
    let mut frame = Frame::new(8, 8, false);
    let mut screen = frame.screen(BLACK);
    assert_eq!(frame.update(&screen, None), PICTURE);
    assert_eq!(frame.update(&screen, None), UNCHANGED);
    screen.set(5, 7, WHITE);
    assert_eq!(frame.update(&screen, None), PICTURE);
    assert_eq!(frame.luma(5, 7), 235);
    assert_eq!(frame.luma(4, 7), 16);
    assert_eq!(frame.update(&screen, None), UNCHANGED);
}

/// A cursor partly off the screen is clipped at each edge.
#[test]
fn the_cursor_is_clipped_at_the_edges() {
    let image = [OPAQUE_WHITE; 16];
    let mut frame = Frame::new(8, 6, false);
    let screen = frame.screen(BLACK);
    for (x, y, inside, outside) in [
        (-2, -3, (1, 0), (2, 1)),
        (6, 4, (7, 5), (5, 3)),
        (-3, 4, (0, 5), (1, 3)),
        (7, -1, (7, 2), (6, 0)),
    ] {
        let cursor = Cursor {
            pixels: &image,
            width: 4,
            x,
            y,
        };
        frame.update(&screen, Some(&cursor));
        assert_eq!(frame.luma(inside.0, inside.1), 235, "cursor at ({x}, {y})");
        assert_eq!(frame.luma(outside.0, outside.1), 16, "cursor at ({x}, {y})");
    }
}

/// The cursor's pixels are premultiplied: a half-transparent black darkens
/// what is under it, and the screen around it keeps its chroma.
#[test]
fn the_cursor_blends_over_the_screen() {
    let mut frame = Frame::new(4, 4, false);
    let screen = frame.screen(WHITE);
    let half_black = 0x8000_0000;
    let cursor = Cursor {
        pixels: &[half_black],
        width: 1,
        x: 3,
        y: 1,
    };
    assert_eq!(frame.update(&screen, Some(&cursor)), PICTURE);
    // Half of the white shows through: 127 of 255, the cursor's alpha being
    // 128.
    let under = 127.0;
    assert_near(
        frame.luma(3, 1),
        bt601(under, under, under).0,
        "Y under the cursor",
    );
    assert_eq!(frame.luma(2, 1), 235);
    assert_eq!(frame.luma(3, 0), 235);
}

/// A cursor that moves leaves the screen as it was behind it, and a frame
/// in which only the cursor moved says so.
#[test]
fn a_moved_cursor_restores_the_screen_behind_it() {
    let image = [OPAQUE_WHITE; 4];
    let mut frame = Frame::new(8, 8, false);
    let screen = frame.screen(BLACK);
    let at = |x, y| Cursor {
        pixels: &image,
        width: 2,
        x,
        y,
    };
    frame.update(&screen, Some(&at(1, 1)));
    assert_eq!(frame.luma(1, 1), 235);
    assert_eq!(frame.update(&screen, Some(&at(1, 1))), UNCHANGED);
    assert_eq!(frame.update(&screen, Some(&at(5, 5))), POINTER);
    assert_eq!(frame.luma(1, 1), 16);
    assert_eq!(frame.chroma(1, 1), (128, 128));
    assert_eq!(frame.luma(6, 6), 235);
    assert_eq!(frame.update(&screen, None), POINTER);
    assert_eq!(frame.luma(6, 6), 16);
}
