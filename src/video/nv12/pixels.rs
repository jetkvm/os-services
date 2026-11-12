//! NV12 pixel operations on Windows: the CPU conversion of a BGRA capture,
//! and the pointer drawn into a frame and removed again. Colors convert with
//! the BT.601 limited-range integer approximation.

use super::Nv12Layout;

/// A rectangle with even edges, so it covers whole chroma samples.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Rect {
    pub left: usize,
    pub top: usize,
    pub width: usize,
    pub height: usize,
}

impl Rect {
    /// The whole frame, without a last odd row or column.
    pub fn whole(layout: &Nv12Layout) -> Self {
        Self {
            left: 0,
            top: 0,
            width: layout.width & !1,
            height: layout.height & !1,
        }
    }

    /// The part of an `width` x `height` box at (`x`, `y`) inside the frame,
    /// widened to even edges; None when nothing is inside.
    pub fn clipped(x: i32, y: i32, width: i32, height: i32, layout: &Nv12Layout) -> Option<Self> {
        let even_edge =
            |value: i32, limit: usize| (value.clamp(0, limit as i32) as usize + 1).min(limit) & !1;
        let left = (x.max(0) as usize) & !1;
        let top = (y.max(0) as usize) & !1;
        let right = even_edge(x + width, layout.width);
        let bottom = even_edge(y + height, layout.height);
        (left < right && top < bottom).then(|| Self {
            left,
            top,
            width: right - left,
            height: bottom - top,
        })
    }
}

pub const BGRA_BYTES: usize = 4;

/// Converts the rectangle of an NV12 frame into BGRA in `out`, rows packed.
pub fn nv12_to_bgra(nv12: &[u8], layout: &Nv12Layout, rect: Rect, out: &mut [u8]) {
    for row in 0..rect.height {
        for column in 0..rect.width {
            let (x, y) = (rect.left + column, rect.top + row);
            let uv = layout.uv_offset + (y / 2) * layout.uv_stride + (x & !1);
            let (blue, green, red) =
                yuv_to_bgr(nv12[y * layout.y_stride + x], nv12[uv], nv12[uv + 1]);
            let pixel = (row * rect.width + column) * BGRA_BYTES;
            out[pixel..pixel + BGRA_BYTES].copy_from_slice(&[blue, green, red, 0xff]);
        }
    }
}

/// Writes BGRA pixels, `bgra_stride` bytes per row, into the rectangle of an
/// NV12 frame. Each chroma sample takes the average of its four pixels.
pub fn bgra_to_nv12(
    bgra: &[u8],
    bgra_stride: usize,
    nv12: &mut [u8],
    layout: &Nv12Layout,
    rect: Rect,
) {
    // Whole rows as slices, which the compiler vectorizes.
    let row_bytes = rect.width * BGRA_BYTES;
    let (luma, chroma) = nv12.split_at_mut(layout.uv_offset);
    for row in 0..rect.height {
        let pixels = &bgra[row * bgra_stride..][..row_bytes];
        let y = &mut luma[(rect.top + row) * layout.y_stride + rect.left..][..rect.width];
        for (y, pixel) in y.iter_mut().zip(pixels.chunks_exact(BGRA_BYTES)) {
            *y = bgr_to_y(pixel[0], pixel[1], pixel[2]);
        }
    }
    for row in (0..rect.height).step_by(2) {
        let top = &bgra[row * bgra_stride..][..row_bytes];
        let bottom = &bgra[(row + 1) * bgra_stride..][..row_bytes];
        let uv = &mut chroma[(rect.top + row) / 2 * layout.uv_stride + rect.left..][..rect.width];
        for ((uv, top), bottom) in uv
            .chunks_exact_mut(2)
            .zip(top.chunks_exact(2 * BGRA_BYTES))
            .zip(bottom.chunks_exact(2 * BGRA_BYTES))
        {
            let average = |channel: usize| {
                let sum = u16::from(top[channel])
                    + u16::from(top[channel + BGRA_BYTES])
                    + u16::from(bottom[channel])
                    + u16::from(bottom[channel + BGRA_BYTES]);
                (sum / 4) as u8
            };
            (uv[0], uv[1]) = bgr_to_uv(average(0), average(1), average(2));
        }
    }
}

/// The Y and U/V bytes of a rectangle, kept so the frame can be put back
/// after the pointer is drawn over it.
#[derive(Default)]
pub struct SavedRegion {
    rect: Option<Rect>,
    bytes: Vec<u8>,
}

impl SavedRegion {
    pub fn save(&mut self, nv12: &[u8], layout: &Nv12Layout, rect: Rect) {
        self.bytes.clear();
        for (start, _) in plane_rows(layout, rect) {
            self.bytes.extend_from_slice(&nv12[start..][..rect.width]);
        }
        self.rect = Some(rect);
    }

    pub fn restore(&mut self, nv12: &mut [u8], layout: &Nv12Layout) {
        let Some(rect) = self.rect.take() else {
            return;
        };
        for (index, (start, _)) in plane_rows(layout, rect).enumerate() {
            nv12[start..][..rect.width]
                .copy_from_slice(&self.bytes[index * rect.width..][..rect.width]);
        }
    }

    pub fn clear(&mut self) {
        self.rect = None;
        self.bytes.clear();
    }
}

/// The byte offset of every row of the rectangle, Y rows then U/V rows, with
/// the row's index within its plane.
fn plane_rows(layout: &Nv12Layout, rect: Rect) -> impl Iterator<Item = (usize, usize)> + '_ {
    let luma =
        (0..rect.height).map(move |row| ((rect.top + row) * layout.y_stride + rect.left, row));
    let chroma = (0..rect.height / 2).map(move |row| {
        (
            layout.uv_offset + (rect.top / 2 + row) * layout.uv_stride + rect.left,
            row,
        )
    });
    luma.chain(chroma)
}

fn clamp_byte(value: i32) -> u8 {
    value.clamp(0, 255) as u8
}

pub fn bgr_to_y(blue: u8, green: u8, red: u8) -> u8 {
    let (blue, green, red) = (i32::from(blue), i32::from(green), i32::from(red));
    clamp_byte(((25 * blue + 129 * green + 66 * red + 128) >> 8) + 16)
}

pub fn bgr_to_uv(blue: u8, green: u8, red: u8) -> (u8, u8) {
    let (blue, green, red) = (i32::from(blue), i32::from(green), i32::from(red));
    let u = clamp_byte(((112 * blue - 74 * green - 38 * red + 128) >> 8) + 128);
    let v = clamp_byte(((-18 * blue - 94 * green + 112 * red + 128) >> 8) + 128);
    (u, v)
}

pub fn yuv_to_bgr(y: u8, u: u8, v: u8) -> (u8, u8, u8) {
    let y = (i32::from(y) - 16).max(0);
    let (u, v) = (i32::from(u) - 128, i32::from(v) - 128);
    let blue = clamp_byte((298 * y + 516 * u + 128) >> 8);
    let green = clamp_byte((298 * y - 100 * u - 208 * v + 128) >> 8);
    let red = clamp_byte((298 * y + 409 * v + 128) >> 8);
    (blue, green, red)
}

#[cfg(test)]
mod tests {
    use super::*;

    const LAYOUT: Nv12Layout = Nv12Layout {
        width: 8,
        height: 4,
        y_stride: 8,
        uv_offset: 32,
        uv_stride: 8,
    };

    fn frame() -> Vec<u8> {
        (0..LAYOUT.buffer_len()).map(|value| value as u8).collect()
    }

    #[test]
    fn primaries_survive_a_round_trip_within_rounding() {
        for bgr in [
            (0, 0, 255),
            (0, 255, 0),
            (255, 0, 0),
            (255, 255, 255),
            (0, 0, 0),
        ] {
            let y = bgr_to_y(bgr.0, bgr.1, bgr.2);
            let (u, v) = bgr_to_uv(bgr.0, bgr.1, bgr.2);
            let back = yuv_to_bgr(y, u, v);
            for (original, converted) in [(bgr.0, back.0), (bgr.1, back.1), (bgr.2, back.2)] {
                assert!(original.abs_diff(converted) <= 3, "{bgr:?} became {back:?}");
            }
        }
    }

    #[test]
    fn limited_range_black_and_white() {
        assert_eq!(bgr_to_y(0, 0, 0), 16);
        assert_eq!(bgr_to_y(255, 255, 255), 235);
        assert_eq!(bgr_to_uv(0, 0, 0), (128, 128));
    }

    #[test]
    fn clipping_widens_to_even_edges_and_stays_inside() {
        assert_eq!(
            Rect::clipped(3, 1, 3, 2, &LAYOUT),
            Some(Rect {
                left: 2,
                top: 0,
                width: 4,
                height: 4,
            })
        );
        assert_eq!(
            Rect::clipped(-5, -5, 7, 7, &LAYOUT),
            Some(Rect {
                left: 0,
                top: 0,
                width: 2,
                height: 2,
            })
        );
        assert_eq!(Rect::clipped(8, 0, 4, 4, &LAYOUT), None);
    }

    #[test]
    fn drawing_touches_only_the_rectangle_and_restores_exactly() {
        let original = frame();
        let mut nv12 = original.clone();
        let rect = Rect {
            left: 2,
            top: 2,
            width: 2,
            height: 2,
        };
        let mut saved = SavedRegion::default();
        saved.save(&nv12, &LAYOUT, rect);
        bgra_to_nv12(&[255; 16], 8, &mut nv12, &LAYOUT, rect);
        let changed: Vec<usize> = (0..nv12.len())
            .filter(|&i| nv12[i] != original[i])
            .collect();
        assert_eq!(changed, [18, 19, 26, 27, 42, 43]);
        saved.restore(&mut nv12, &LAYOUT);
        assert_eq!(nv12, original);
    }

    #[test]
    fn a_region_read_back_as_bgra_keeps_its_colors() {
        let mut nv12 = vec![0; LAYOUT.buffer_len()];
        let rect = Rect {
            left: 0,
            top: 0,
            width: 2,
            height: 2,
        };
        let red = [0, 0, 255, 0xff].repeat(4);
        bgra_to_nv12(&red, 8, &mut nv12, &LAYOUT, rect);
        let mut bgra = [0; 16];
        nv12_to_bgra(&nv12, &LAYOUT, rect, &mut bgra);
        for (converted, original) in bgra.iter().zip(&red) {
            assert!(converted.abs_diff(*original) <= 3, "{bgra:?}");
        }
    }

    #[test]
    fn a_cleared_region_restores_nothing() {
        let mut nv12 = frame();
        let mut saved = SavedRegion::default();
        let rect = Rect {
            left: 0,
            top: 0,
            width: 2,
            height: 2,
        };
        saved.save(&nv12, &LAYOUT, rect);
        nv12[0] = 99;
        saved.clear();
        saved.restore(&mut nv12, &LAYOUT);
        assert_eq!(nv12[0], 99);
    }

    #[test]
    fn a_strided_frame_converts_and_its_padding_is_ignored() {
        let layout = Nv12Layout {
            width: 4,
            height: 2,
            y_stride: 4,
            uv_offset: 8,
            uv_stride: 4,
        };
        let white = [255, 255, 255, 0xff];
        let red = [0, 0, 255, 0xff];
        let row: Vec<u8> = [white, white, red, red].concat();
        let padded: Vec<u8> = [row.clone(), vec![0xee; 4], row].concat();
        let mut nv12 = vec![0; layout.buffer_len()];
        bgra_to_nv12(&padded, 20, &mut nv12, &layout, Rect::whole(&layout));
        assert_eq!(
            nv12,
            [235, 235, 82, 82, 235, 235, 82, 82, 128, 128, 90, 240]
        );
    }
}
