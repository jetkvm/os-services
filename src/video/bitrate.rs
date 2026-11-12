//! The default bitrate of a stream.

use crate::protocol::VideoCodec;

/// The quality ratio the default bitrate derives from.
const QUALITY: f32 = 1.8;

/// The default bitrate in kbit/s for a codec at a size: a base rate that
/// grows with the pixel count, scaled by QUALITY, with more headroom at low
/// rates and for H.264, which needs more bits than H.265 for the same image.
pub fn default_kbps(codec: VideoCodec, width: usize, height: usize) -> u32 {
    let base = base_kbps(width as u64 * height as u64) * QUALITY;
    let (low_rate_factor, headroom) = match codec {
        VideoCodec::H264 => (2.0, 1.0),
        VideoCodec::H265 => (1.5, 0.5),
    };
    const THRESHOLD: f32 = 2000.0;
    let factor = if base > THRESHOLD {
        1.0 + headroom / (1.0 + (base - THRESHOLD) * 0.001)
    } else {
        low_rate_factor
    };
    (base * factor) as u32
}

/// The base rate at a pixel count: a line between the two presets around it,
/// so a larger frame never gets a lower rate. Below the smallest preset and
/// above the largest, the rate is that preset's, scaled to the pixel count.
fn base_kbps(pixels: u64) -> f32 {
    const PRESETS: [(u64, u64, f32); 13] = [
        (640, 480, 400.0),
        (800, 600, 500.0),
        (1024, 768, 800.0),
        (1280, 720, 1000.0),
        (1366, 768, 1100.0),
        (1440, 900, 1300.0),
        (1600, 900, 1500.0),
        (1920, 1080, 2073.0),
        (2048, 1080, 2200.0),
        (2560, 1440, 3000.0),
        (3440, 1440, 4000.0),
        (3840, 2160, 5000.0),
        (7680, 4320, 12000.0),
    ];
    let preset = |index: usize| {
        let (width, height, kbps) = PRESETS[index];
        ((width * height) as f32, kbps)
    };
    let pixels = pixels as f32;
    let above = PRESETS
        .iter()
        .position(|&(width, height, _)| (width * height) as f32 >= pixels);
    let kbps = match above {
        Some(0) => preset(0).1 * pixels / preset(0).0,
        Some(index) => {
            let (low_pixels, low_kbps) = preset(index - 1);
            let (high_pixels, high_kbps) = preset(index);
            low_kbps + (high_kbps - low_kbps) * (pixels - low_pixels) / (high_pixels - low_pixels)
        }
        None => {
            let (last_pixels, last_kbps) = preset(PRESETS.len() - 1);
            last_kbps * pixels / last_pixels
        }
    };
    kbps.round()
}

#[cfg(test)]
mod tests {
    use super::default_kbps;
    use crate::protocol::VideoCodec;

    #[test]
    fn the_rate_grows_with_the_size_and_h265_needs_less() {
        let sizes = [(1280, 720), (1920, 1080), (2560, 1440), (3840, 2160)];
        for codec in [VideoCodec::H264, VideoCodec::H265] {
            let rates: Vec<u32> = sizes
                .iter()
                .map(|&(w, h)| default_kbps(codec, w, h))
                .collect();
            assert!(
                rates.windows(2).all(|pair| pair[0] < pair[1]),
                "{codec:?}: {rates:?}"
            );
        }
        for (width, height) in sizes {
            assert!(
                default_kbps(VideoCodec::H265, width, height)
                    < default_kbps(VideoCodec::H264, width, height)
            );
        }
    }

    #[test]
    fn a_larger_frame_never_gets_a_lower_rate() {
        for codec in [VideoCodec::H264, VideoCodec::H265] {
            let mut previous = (0, 0, 0);
            for width in (320..=8192).step_by(16) {
                let height = width * 9 / 16;
                let kbps = default_kbps(codec, width, height);
                assert!(
                    kbps >= previous.2,
                    "{codec:?}: {width}x{height} gets {kbps} kbit/s, {}x{} gets {}",
                    previous.0,
                    previous.1,
                    previous.2
                );
                previous = (width, height, kbps);
            }
        }
    }
}
