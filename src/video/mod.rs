//! The video service: display selection, capture, encoding and the stream
//! the KVM receives. Linux captures the scanout through KMS and encodes it
//! with VAAPI or NVENC without a CPU copy (`kms`); Windows and macOS capture into RAM
//! and encode with a hardware encoder (`ram`).

pub mod annex_b;
pub mod bitrate;
mod capture_failure;
pub mod display;
#[cfg(target_os = "linux")]
pub mod kms;
#[cfg(target_os = "linux")]
pub mod missing_encoder;
#[cfg(any(not(target_os = "linux"), test))]
pub(crate) mod nv12;
#[cfg(not(target_os = "linux"))]
pub mod ram;
pub mod stream;
pub mod telemetry;

pub use capture_failure::CaptureFailure;
#[cfg(target_os = "linux")]
pub use kms::{KmsPipeline as PlatformPipeline, Source};
#[cfg(not(target_os = "linux"))]
pub use ram::{RamPipeline as PlatformPipeline, Source};

use crate::protocol::VideoConfig;
use crate::session::worker::{Wire, WorkerFailure};
use std::time::{Duration, Instant};

/// The native capture rate.
pub const FPS: u64 = 120;
pub const FRAME_INTERVAL: Duration = Duration::from_nanos(1_000_000_000 / FPS);
/// How long a new capture or encoder has to produce its first frame.
pub const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(10);

/// Recovery IDRs answer keyframe requests at most this often. The KVM
/// repeats a request while its decoder waits, and keying again inside the
/// window starts a request, IDR, burst, stall, request loop under
/// congestion.
const RECOVERY_IDR_HOLDOFF: Duration = Duration::from_millis(700);

/// True while the last recovery IDR is recent enough to answer a keyframe
/// request by itself.
pub fn within_idr_holdoff(last_idr: Option<Instant>) -> bool {
    last_idr.is_some_and(|at| at.elapsed() < RECOVERY_IDR_HOLDOFF)
}

/// The quality fields of a video.config. `kbps` overrides the default
/// bitrate; `max_fps` caps the frame rate below the native rate. None means
/// the encoder default.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct QualityTarget {
    pub kbps: Option<u32>,
    pub max_fps: Option<u64>,
}

impl QualityTarget {
    pub fn from_config(config: VideoConfig) -> Self {
        let max_fps = u64::from(config.maximum_fps);
        Self {
            kbps: (config.target_kbps != 0).then_some(config.target_kbps),
            max_fps: (max_fps != 0 && max_fps < FPS).then_some(max_fps),
        }
    }

    pub fn effective_fps(&self) -> u64 {
        self.max_fps.unwrap_or(FPS)
    }

    /// The requested bitrate, capped at twice the default.
    pub fn effective_kbps(&self, default_kbps: u32) -> u32 {
        self.kbps.map_or(default_kbps, |kbps| {
            kbps.min(default_kbps.saturating_mul(2))
        })
    }

    /// The video.status target_kbps field: 0 where the default applies.
    pub fn applied_kbps(&self, default_kbps: u32) -> u32 {
        self.kbps.map_or(0, |_| self.effective_kbps(default_kbps))
    }

    /// The video.status maximum_fps field: 0 where the native rate applies.
    pub fn applied_maximum_fps(&self) -> u32 {
        self.max_fps.map_or(0, |fps| fps as u32)
    }
}

/// What the streaming thread asks the pipeline to encode.
pub struct EncoderRequest {
    /// The display the stream shows. The KMS pipeline captures by it; the
    /// RAM pipelines encode what the streaming thread captures.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub display: String,
    pub width: usize,
    pub height: usize,
    pub config: VideoConfig,
}

/// The encoder a pipeline configured.
pub struct Configured {
    pub bitrate: u32,
    /// The NV12 layout the encoder reads, which the capture writes.
    #[cfg(not(target_os = "linux"))]
    pub layout: nv12::Nv12Layout,
}

/// What one pass of the streaming thread's `Source` did. Each platform's
/// `Source` has the same methods: `open`, `next`, `reopen`, `close`,
/// `set_configured`, `keyframe_requested`, `stream_reset`,
/// `needs_new_capturer` and `renew`.
// Each platform produces only some of these: a KMS pass is always Idle, and
// only Windows recovers from an invalidated capturer.
#[cfg_attr(not(windows), allow(dead_code))]
pub enum Captured {
    /// A frame went to the worker.
    Frame,
    /// Nothing new on the desktop.
    Idle,
    /// The display no longer matches the capture: place it again.
    DisplayChanged,
    /// Windows invalidated the DXGI capture, and capture went on with GDI;
    /// capture again.
    Recovered,
}

/// Paces the KMS streaming thread to the native frame rate. The Windows and
/// macOS capturers wait for frames themselves.
#[cfg(target_os = "linux")]
pub struct FramePacer {
    next_frame: Instant,
}

#[cfg(target_os = "linux")]
impl FramePacer {
    pub fn new() -> Self {
        Self {
            next_frame: Instant::now(),
        }
    }

    /// Sleeps until the next frame is due. A late loop resumes from now
    /// instead of running several frames back to back.
    pub fn wait(&mut self) {
        let now = Instant::now();
        if now < self.next_frame {
            std::thread::sleep(self.next_frame - now);
        }
        self.next_frame = (self.next_frame + FRAME_INTERVAL).max(Instant::now());
    }
}

/// A video pipeline runs on the session's worker thread, which owns the
/// wire. The worker hands it the streaming thread's commands and calls
/// `step` between command passes.
pub trait Pipeline {
    /// Creates the encoder for a display and announces the new stream.
    fn configure(
        &mut self,
        request: EncoderRequest,
        wire: &mut Wire,
    ) -> Result<Configured, WorkerFailure>;
    /// Applies a new bitrate or frame-rate cap; true when the stream was
    /// reset for it.
    fn apply_quality(
        &mut self,
        config: VideoConfig,
        wire: &mut Wire,
    ) -> Result<bool, WorkerFailure>;
    /// Drops the encoder and any capture.
    fn stop(&mut self);
    fn keyframe_request(&mut self);
    /// Encodes and writes what is ready, or pings when nothing is. With
    /// `commands_left` the worker has more commands to serve, so it does not
    /// wait for a frame.
    fn step(&mut self, wire: &mut Wire, commands_left: bool);
    /// How long the worker may wait for a command before the next `step`.
    /// A pipeline that waits for frames in `step` is woken by each command
    /// and needs none.
    fn command_wait(&self) -> Duration {
        Duration::ZERO
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{DisplayLayout, VideoCodec};

    fn config(target_kbps: u32, maximum_fps: u32) -> VideoConfig {
        VideoConfig {
            video_enabled: true,
            layout: DisplayLayout::Mirror,
            codec: VideoCodec::H264,
            width: 1920,
            height: 1080,
            target_kbps,
            maximum_fps,
        }
    }

    #[test]
    fn zero_fields_select_the_encoder_defaults() {
        let quality = QualityTarget::from_config(config(0, 0));
        assert_eq!(quality, QualityTarget::default());
        assert_eq!(quality.effective_fps(), FPS);
        assert_eq!(quality.effective_kbps(9_000), 9_000);
        assert_eq!(
            (quality.applied_kbps(9_000), quality.applied_maximum_fps()),
            (0, 0)
        );
    }

    #[test]
    fn a_frame_rate_cap_at_or_above_native_is_no_cap() {
        assert_eq!(QualityTarget::from_config(config(0, 120)).max_fps, None);
        assert_eq!(
            QualityTarget::from_config(config(0, 60)).effective_fps(),
            60
        );
    }

    #[test]
    fn requested_bitrate_is_capped_at_twice_the_default() {
        let quality = QualityTarget::from_config(config(30_000, 0));
        assert_eq!(quality.effective_kbps(9_000), 18_000);
        assert_eq!(quality.applied_kbps(9_000), 18_000);
        let modest = QualityTarget::from_config(config(4_000, 0));
        assert_eq!(modest.effective_kbps(9_000), 4_000);
    }
}
