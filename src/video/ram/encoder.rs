//! Hardware encoding of RAM frames with FFmpeg's AMF, NVENC, QSV and
//! VideoToolbox encoders (`ffmpeg`).

use super::ffmpeg::{self, Codec, Packet};
use crate::protocol::VideoCodec;
use crate::session::worker::{Wire, WorkerFailure};
use crate::video::{
    annex_b::{extract_parameter_sets, is_annex_b, unit_has_idr, unit_has_parameter_sets},
    CaptureFailure, QualityTarget,
};
use crate::video::{bitrate, nv12::Nv12Layout};
use std::{
    error::Error,
    sync::Mutex,
    time::{Duration, Instant},
};

/// The FFmpeg names of the hardware encoders this machine has. H.265 is
/// optional and offered to the KVM only when present.
#[derive(Clone)]
pub struct EncoderCodecs {
    pub h264: String,
    pub h265: Option<String>,
}

/// The encoders the last probe found. A later session starts video with them
/// and skips the probe, which takes seconds (2.2 s with AMF). An encoder that
/// fails to open or to encode clears them, so the next probe looks again and
/// a session does not register video on a stale answer.
static FOUND: Mutex<Option<EncoderCodecs>> = Mutex::new(None);

/// Finds the hardware encoders, or reports that there is no H.264 one.
pub fn probe() -> Result<EncoderCodecs, CaptureFailure> {
    if let Some(found) = FOUND.lock().unwrap().clone() {
        return Ok(found);
    }
    let h264 = ffmpeg::first_encoder("h264").ok_or(CaptureFailure::EncoderUnavailable)?;
    let h265 = ffmpeg::first_encoder("hevc");
    if let Some(h265) = &h265 {
        eprintln!("hardware H.265 encoder available: {h265}");
    }
    let found = EncoderCodecs { h264, h265 };
    *FOUND.lock().unwrap() = Some(found.clone());
    Ok(found)
}

fn forget_probe() {
    *FOUND.lock().unwrap() = None;
}

impl EncoderCodecs {
    /// The codec this machine encodes for a requested one.
    pub fn resolve(&self, requested: VideoCodec) -> VideoCodec {
        match requested {
            VideoCodec::H265 if self.h265.is_none() => {
                eprintln!("H.265 requested but no hardware encoder; using H.264");
                VideoCodec::H264
            }
            other => other,
        }
    }

    pub fn open(
        &self,
        codec: VideoCodec,
        (width, height): (usize, usize),
        quality: QualityTarget,
    ) -> Result<VideoEncoder, Box<dyn Error>> {
        let name = match codec {
            VideoCodec::H265 => self.h265.as_ref().expect("resolve returned H.265"),
            VideoCodec::H264 => &self.h264,
        };
        VideoEncoder::new(name, codec, width, height, quality).inspect_err(|_| forget_probe())
    }
}

pub struct VideoEncoder {
    inner: Codec,
    codec: VideoCodec,
    pub bitrate: u32,
    pub default_kbps: u32,
    first: bool,
    parameter_sets: Vec<u8>,
    /// An IDR with its parameter sets prepended; reused so a keyframe costs
    /// no allocation.
    repeated: Vec<u8>,
    pub last_encode: Duration,
}

impl VideoEncoder {
    fn new(
        name: &str,
        codec: VideoCodec,
        width: usize,
        height: usize,
        quality: QualityTarget,
    ) -> Result<Self, Box<dyn Error>> {
        let default_kbps = bitrate::default_kbps(codec, width, height);
        let bitrate = quality.effective_kbps(default_kbps);
        let fps = quality.effective_fps();
        let inner = Codec::open(name, (width, height), fps, bitrate)
            .ok_or("failed to create the hardware video encoder")?;
        Ok(Self {
            inner,
            codec,
            bitrate,
            default_kbps,
            first: true,
            parameter_sets: Vec::new(),
            repeated: Vec::new(),
            last_encode: Duration::ZERO,
        })
    }

    /// Makes the next frame an IDR. The encoder schedules no IDR: the KVM
    /// asks for a keyframe when a viewer connects and when frames are lost
    /// (PROTOCOL.md), as on the KMS path, and a scheduled IDR of a detailed
    /// desktop (about 500 KB at 1080p) holds the next frames on a KVM's USB
    /// link longer than a viewer waits before it counts a freeze.
    pub fn request_keyframe(&mut self) {
        self.inner.request_keyframe();
    }

    /// Changes the bitrate of the running encoder.
    pub fn set_bitrate(&mut self, kbps: u32) {
        self.inner.set_bitrate(kbps);
        self.bitrate = kbps;
    }

    /// The NV12 layout the encoder reads.
    pub fn nv12_layout(&self) -> Nv12Layout {
        self.inner.layout
    }

    pub fn encode(&mut self, yuv: &[u8], pts_ms: i64) -> Result<Vec<Packet>, Box<dyn Error>> {
        let started = Instant::now();
        let packets = self.inner.encode(yuv, pts_ms).map_err(|code| {
            forget_probe();
            format!("hardware video encode failed: {code}")
        })?;
        self.last_encode = started.elapsed();
        Ok(packets)
    }

    /// Writes the encoded packets as access units; returns the bytes written
    /// and whether one was a keyframe. Every IDR carries its parameter sets
    /// so a decoder can join at any keyframe. A failure here ends the
    /// session, as a wire failure does.
    pub fn write(
        &mut self,
        packets: Vec<Packet>,
        wire: &mut Wire,
        capture_us: u64,
    ) -> Result<(usize, bool), WorkerFailure> {
        let (mut bytes, mut keyframe) = (0, false);
        for Packet { data: unit, .. } in packets {
            if !is_annex_b(&unit) {
                return Err(wire.fail("the encoder wrote a packet without start codes"));
            }
            let idr = unit_has_idr(self.codec, &unit);
            let has_parameter_sets = unit_has_parameter_sets(self.codec, &unit);
            let unit = self.with_parameter_sets(&unit, idr, has_parameter_sets, wire)?;
            wire.write_access_unit(unit, capture_us, idr, has_parameter_sets || idr)?;
            bytes += unit.len();
            keyframe |= idr;
        }
        Ok((bytes, keyframe))
    }

    /// The unit to send: the first one must be an IDR with parameter sets,
    /// which are kept and prepended to every later IDR that lacks them.
    fn with_parameter_sets<'a>(
        &'a mut self,
        unit: &'a [u8],
        idr: bool,
        has_parameter_sets: bool,
        wire: &Wire,
    ) -> Result<&'a [u8], WorkerFailure> {
        if self.first {
            if !has_parameter_sets || !idr {
                return Err(
                    wire.fail("first access unit does not contain parameter sets and an IDR")
                );
            }
            self.parameter_sets = extract_parameter_sets(self.codec, unit);
            self.first = false;
            return Ok(unit);
        }
        if !idr || has_parameter_sets {
            return Ok(unit);
        }
        self.repeated.clear();
        self.repeated.extend_from_slice(&self.parameter_sets);
        self.repeated.extend_from_slice(unit);
        Ok(&self.repeated)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The encoders the probe finds on this computer, with their codecs.
    fn probed() -> Vec<(String, VideoCodec)> {
        let codecs = probe().expect("a hardware H.264 encoder");
        let mut found = vec![(codecs.h264, VideoCodec::H264)];
        found.extend(codecs.h265.map(|name| (name, VideoCodec::H265)));
        found
    }

    #[test]
    #[ignore = "requires a hardware H.264 encoder"]
    fn encoders_deliver_each_frame() {
        for (name, codec) in probed() {
            let quality = QualityTarget {
                kbps: Some(4000),
                max_fps: Some(60),
            };
            let mut encoder =
                VideoEncoder::new(&name, codec, 1280, 720, quality).expect("encoder opens");
            let mut pixels = vec![128; encoder.inner.layout.buffer_len()];
            for pts in [0, 17, 34, 51, 2000, 2017] {
                pixels[..1280].fill((pts % 200 + 16) as u8);
                let packets = encoder.encode(&pixels, pts).expect("frame encodes");
                // One packet for each frame, from the first on, so the
                // encoder holds no frame back.
                assert_eq!(packets.len(), 1, "{name} withheld frame {pts}");
                assert!(!packets[0].data.is_empty());
                // No later input releases this frame; the encoder must stay
                // usable after an idle desktop resumes.
                if pts == 51 {
                    std::thread::sleep(Duration::from_millis(100));
                    encoder.set_bitrate(3000);
                    encoder.request_keyframe();
                }
            }
        }
    }

    #[test]
    #[ignore = "requires a hardware H.264 encoder"]
    fn encoders_send_an_idr_only_on_request() {
        for (name, codec) in probed() {
            let quality = QualityTarget {
                kbps: Some(4000),
                max_fps: Some(30),
            };
            let mut encoder =
                VideoEncoder::new(&name, codec, 1280, 720, quality).expect("encoder opens");
            let mut pixels = vec![128; encoder.inner.layout.buffer_len()];
            // A new bitrate applies to the running session, without an IDR.
            let idrs: Vec<i64> = (0..200)
                .filter(|&frame| {
                    if frame == 60 {
                        encoder.request_keyframe();
                    }
                    if frame == 120 {
                        encoder.set_bitrate(2000);
                    }
                    pixels[..1280].fill((frame % 200 + 16) as u8);
                    let packets = encoder.encode(&pixels, frame * 33).expect("frame encodes");
                    packets
                        .into_iter()
                        .any(|packet| unit_has_idr(codec, &packet.data))
                })
                .collect();
            assert_eq!(idrs, [0, 60], "{name} sent IDRs at other frames");
        }
    }

    /// The encoded size of each of `frames` frames of noise from an encoder
    /// opened at 8000 kbit/s, which changes to 1000 kbit/s before frame
    /// `change`. Each frame needs more bits than either bitrate allows, so
    /// the sizes follow the bitrate. The noise spans 32 levels: full-range
    /// noise needs more than 1000 kbit/s on hevc_qsv even at its coarsest
    /// quantizer.
    fn noise_sizes(name: &str, codec: VideoCodec, frames: i64, change: Option<i64>) -> Vec<usize> {
        let quality = QualityTarget {
            kbps: Some(8000),
            max_fps: Some(30),
        };
        let mut encoder =
            VideoEncoder::new(name, codec, 1280, 720, quality).expect("encoder opens");
        let mut pixels = vec![128; encoder.inner.layout.buffer_len()];
        let mut state = 0x2545_f491_4f6c_dd1d_u64;
        (0..frames)
            .map(|frame| {
                if change == Some(frame) {
                    encoder.set_bitrate(1000);
                }
                for byte in &mut pixels[..1280 * 720] {
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    *byte = 112 + (state as u8 & 31);
                }
                let packets = encoder.encode(&pixels, frame * 33).expect("frame encodes");
                packets.iter().map(|packet| packet.data.len()).sum()
            })
            .collect()
    }

    #[test]
    #[ignore = "requires a hardware H.264 encoder"]
    fn encoders_apply_a_new_bitrate() {
        for (name, codec) in probed() {
            let sizes = noise_sizes(&name, codec, 120, Some(60));
            let before: usize = sizes[30..60].iter().sum();
            let after: usize = sizes[90..120].iter().sum();
            assert!(
                after * 2 < before,
                "{name} kept its bitrate: {before} bytes in 30 frames at 8000 kbit/s, {after} at 1000 kbit/s"
            );
        }
    }

    #[test]
    #[ignore = "requires a hardware H.264 encoder"]
    fn encoders_apply_a_bitrate_set_before_the_first_frame() {
        for (name, codec) in probed() {
            let kept: usize = noise_sizes(&name, codec, 60, None)[30..].iter().sum();
            let changed: usize = noise_sizes(&name, codec, 60, Some(0))[30..].iter().sum();
            assert!(
                changed * 2 < kept,
                "{name} kept its bitrate: {changed} bytes in 30 frames after a change to 1000 kbit/s before the first frame, {kept} without it"
            );
        }
    }
}
