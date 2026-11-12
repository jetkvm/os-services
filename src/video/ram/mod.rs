//! The Windows and macOS pipeline. The streaming thread captures and
//! converts frames into NV12; the worker encodes them with a hardware
//! encoder. A one-frame mailbox between them replaces a stale raw frame when
//! encoding falls behind, which bounds latency without dropping encoded
//! frames that later frames depend on.

#[cfg(target_os = "macos")]
#[path = "capture_macos.rs"]
mod capture;
#[cfg(windows)]
#[path = "capture_windows.rs"]
mod capture;
#[cfg(windows)]
mod cursor;
#[cfg(windows)]
mod dxgi;
mod encoder;
mod ffmpeg;

pub use capture::Source;
pub use encoder::{probe, EncoderCodecs};

use super::{
    telemetry::{FrameSample, StageWindow},
    within_idr_holdoff, Configured, EncoderRequest, Pipeline, QualityTarget, FIRST_FRAME_TIMEOUT,
};
use crate::protocol::{VideoCodec, VideoConfig};
use crate::session::worker::{FailureSlot, Wire, WorkerFailure};
use encoder::VideoEncoder;
use std::{
    sync::{
        atomic::{AtomicBool, AtomicU32, Ordering},
        Arc, Condvar, Mutex,
    },
    time::{Duration, Instant},
};

/// How long the worker waits for a frame before it pings instead.
const FRAME_WAIT: Duration = Duration::from_millis(250);

/// One captured frame, converted and ready to encode.
pub struct FrameJob {
    pub nv12: Vec<u8>,
    pub width: usize,
    pub height: usize,
    pub capture_us: u64,
    pub capture_wait: Duration,
    pub convert: Duration,
    /// False when the frame repeats the last capture with only the pointer
    /// redrawn.
    pub fresh: bool,
    /// The frame answers a keyframe request on an idle desktop: the worker
    /// keys it.
    pub keyframe_resend: bool,
}

/// Capture timestamps and the first-frame deadline of a capture.
struct FrameClock {
    /// Timestamps count from here across every reconfigure, so the
    /// encoder's pts never runs backwards.
    epoch: Instant,
    first_frame_deadline: Instant,
}

impl FrameClock {
    fn new() -> Self {
        let now = Instant::now();
        Self {
            epoch: now,
            first_frame_deadline: now + FIRST_FRAME_TIMEOUT,
        }
    }

    /// A new capture epoch: the first frame is awaited again.
    fn restart(&mut self) {
        self.first_frame_deadline = Instant::now() + FIRST_FRAME_TIMEOUT;
    }

    /// The capture time of a pass that started at `started`, in
    /// microseconds since the epoch, and how long the pass waited.
    fn pass(&self, started: Instant) -> (u64, Duration) {
        let now = Instant::now();
        ((now - self.epoch).as_micros() as u64, now - started)
    }

    /// Fails a capture that has produced no frame by its deadline.
    fn check_first_frame(&self, have_frame: bool) -> Result<(), &'static str> {
        if !have_frame && Instant::now() >= self.first_frame_deadline {
            return Err("capture produced no first frame before timeout");
        }
        Ok(())
    }
}

/// The hand-over between the capture thread and the worker: the one-frame
/// slot, the buffers that cycle between them so sustained displacement costs
/// no new multi-megabyte allocations, and the counters telemetry reads.
#[derive(Default)]
pub struct FrameMailbox {
    slot: Mutex<Option<FrameJob>>,
    available: Condvar,
    /// A command was queued since the worker last waited: the next wait
    /// returns at once instead of missing a wake that came before it.
    woken: AtomicBool,
    buffers: Mutex<Vec<Vec<u8>>>,
    /// Capture passes that produced no frame.
    idle: AtomicU32,
    /// Jobs a newer frame displaced before the worker took them.
    late: AtomicU32,
}

impl FrameMailbox {
    pub fn push(&self, job: FrameJob) {
        let displaced = self.slot.lock().unwrap().replace(job);
        self.available.notify_one();
        if let Some(displaced) = displaced {
            self.late.fetch_add(1, Ordering::Relaxed);
            self.recycle(displaced.nv12);
        }
    }

    /// Ends the worker's wait for a frame so a queued command is served now.
    /// Under the slot's lock, so a worker about to wait either sees the flag
    /// or is waiting when the notification comes.
    pub fn wake(&self) {
        let _slot = self.slot.lock().unwrap();
        self.woken.store(true, Ordering::Relaxed);
        self.available.notify_one();
    }

    /// The next frame, or None when none arrives within `timeout` or a
    /// command was queued.
    fn pop_timeout(&self, timeout: Duration) -> Option<FrameJob> {
        let slot = self.slot.lock().unwrap();
        // A condition variable can wake before the timeout with nothing to
        // report; only a frame or a wake ends the wait early.
        let (mut slot, _) = self
            .available
            .wait_timeout_while(slot, timeout, |slot| {
                slot.is_none() && !self.woken.load(Ordering::Relaxed)
            })
            .unwrap();
        self.woken.store(false, Ordering::Relaxed);
        slot.take()
    }

    /// A recycled NV12 buffer, or a new empty one.
    pub fn take_buffer(&self) -> Vec<u8> {
        self.buffers.lock().unwrap().pop().unwrap_or_default()
    }

    fn recycle(&self, buffer: Vec<u8>) {
        self.buffers.lock().unwrap().push(buffer);
    }

    pub fn note_idle(&self) {
        self.idle.fetch_add(1, Ordering::Relaxed);
    }

    fn take_counts(&self) -> (u32, u32) {
        (
            self.idle.swap(0, Ordering::Relaxed),
            self.late.swap(0, Ordering::Relaxed),
        )
    }
}

pub struct RamPipeline {
    frames: Arc<FrameMailbox>,
    fault: FailureSlot,
    /// The encoders the capture probe found; None until attached.
    codecs: Option<EncoderCodecs>,
    encoder: Option<VideoEncoder>,
    dims: (usize, usize),
    quality: QualityTarget,
    codec: VideoCodec,
    /// When the frame-rate cap next lets a frame through.
    next_emit: Option<Instant>,
    last_idr: Option<Instant>,
    stages: StageWindow,
}

impl RamPipeline {
    pub fn new(frames: Arc<FrameMailbox>, fault: FailureSlot) -> Self {
        Self {
            frames,
            fault,
            codecs: None,
            encoder: None,
            dims: (0, 0),
            quality: QualityTarget::default(),
            codec: VideoCodec::H264,
            next_emit: None,
            last_idr: None,
            stages: StageWindow::new(),
        }
    }

    pub fn attach(&mut self, codecs: EncoderCodecs) {
        self.codecs = Some(codecs);
    }

    /// A new encoder for the current stream, with `quality`.
    fn open_encoder(&self, quality: QualityTarget) -> Result<VideoEncoder, String> {
        let codecs = self.codecs.as_ref().ok_or("no capture backend attached")?;
        codecs
            .open(self.codec, self.dims, quality)
            .map_err(|err| err.to_string())
    }

    /// Replaces the encoder; the next frame is its first and passes the
    /// frame-rate cap.
    fn replace_encoder(&mut self, encoder: VideoEncoder) {
        self.encoder = Some(encoder);
        self.next_emit = None;
    }

    /// Frames the cap drops return false.
    fn frame_rate_allows(&mut self, job: &FrameJob) -> bool {
        let Some(cap) = self.quality.max_fps else {
            return true;
        };
        let now = Instant::now();
        if self.next_emit.is_some_and(|next| now < next) {
            self.stages.record_capped(job.capture_wait);
            return false;
        }
        let scheduled = self.next_emit.unwrap_or(now) + Duration::from_nanos(1_000_000_000 / cap);
        self.next_emit = Some(scheduled.max(now));
        true
    }

    fn encode(&mut self, job: &FrameJob, wire: &mut Wire) {
        // A frame with other dimensions predates a reconfigure.
        if wire.failed() || self.encoder.is_none() || (job.width, job.height) != self.dims {
            return;
        }
        // The request a re-sent frame answers travels on the command channel
        // and can still be queued behind the frame, so it is applied here.
        if job.keyframe_resend && !within_idr_holdoff(self.last_idr) {
            let active = self.encoder.as_mut().expect("checked above");
            active.request_keyframe();
        }
        if !self.frame_rate_allows(job) {
            return;
        }
        let active = self.encoder.as_mut().expect("checked above");
        let packets = match active.encode(&job.nv12, (job.capture_us / 1_000) as i64) {
            Ok(packets) => packets,
            Err(err) => {
                // The encoder failed, not the wire: video ends, the session
                // does not.
                self.encoder = None;
                self.fault.set(err.to_string());
                return;
            }
        };
        let write_started = Instant::now();
        let Ok((bytes, keyframe)) = active.write(packets, wire, job.capture_us) else {
            return;
        };
        if bytes == 0 {
            return;
        }
        if keyframe {
            self.last_idr = Some(Instant::now());
        }
        self.stages.record_frame(FrameSample {
            capture_wait: job.capture_wait,
            convert: job.convert,
            encode: active.last_encode,
            write: write_started.elapsed(),
            bytes,
            keyframe,
            fresh: job.fresh,
        });
    }
}

impl Pipeline for RamPipeline {
    fn configure(
        &mut self,
        request: EncoderRequest,
        wire: &mut Wire,
    ) -> Result<Configured, WorkerFailure> {
        let Some(codecs) = self.codecs.as_ref() else {
            return Err("no capture backend attached".into());
        };
        self.codec = codecs.resolve(request.config.codec);
        self.dims = (request.width, request.height);
        self.quality = QualityTarget::from_config(request.config);
        let encoder = self
            .open_encoder(self.quality)
            .map_err(|err| self.fault.record(err))?;
        wire.announce_stream(
            request.config,
            self.codec,
            self.dims,
            encoder.default_kbps,
            self.quality,
            true,
        )?;
        let configured = Configured {
            bitrate: encoder.bitrate,
            layout: encoder.nv12_layout(),
        };
        self.encoder = Some(encoder);
        Ok(configured)
    }

    /// A bitrate-only change is applied to the running encoder; anything
    /// else replaces the encoder and resets the stream.
    fn apply_quality(
        &mut self,
        config: VideoConfig,
        wire: &mut Wire,
    ) -> Result<bool, WorkerFailure> {
        let quality = QualityTarget::from_config(config);
        let bitrate_only = self.quality.max_fps == quality.max_fps;
        let replacement = match self.encoder.as_mut() {
            Some(active) if bitrate_only => {
                active.set_bitrate(quality.effective_kbps(active.default_kbps));
                None
            }
            _ => Some(
                self.open_encoder(quality)
                    .map_err(|err| self.fault.record(err))?,
            ),
        };
        let default_kbps = replacement
            .as_ref()
            .or(self.encoder.as_ref())
            .expect("a running or replacement encoder")
            .default_kbps;
        let reset = replacement.is_some();
        wire.announce_stream(config, self.codec, self.dims, default_kbps, quality, reset)?;
        self.quality = quality;
        if let Some(encoder) = replacement {
            self.replace_encoder(encoder);
        }
        Ok(reset)
    }

    fn stop(&mut self) {
        self.encoder = None;
        self.dims = (0, 0);
        self.next_emit = None;
    }

    fn keyframe_request(&mut self) {
        if within_idr_holdoff(self.last_idr) {
            return;
        }
        // Without an encoder there is no stream to key: either video is
        // stopped, or the encoder failed and that fault ends the stream.
        if let Some(active) = self.encoder.as_mut() {
            active.request_keyframe();
            eprintln!("forced IDR for keyframe request");
        }
    }

    fn step(&mut self, wire: &mut Wire, commands_left: bool) {
        let wait = if commands_left {
            Duration::ZERO
        } else {
            FRAME_WAIT
        };
        match self.frames.pop_timeout(wait) {
            None => wire.ping_if_due(),
            Some(job) => {
                self.encode(&job, wire);
                self.frames.recycle(job.nv12);
            }
        }
        let (idle, late) = self.frames.take_counts();
        self.stages.add_idle(idle);
        self.stages.add_late(late);
        self.stages.maybe_report();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A command queued while the worker serves the ones before it wakes
    /// the worker before it starts to wait for a frame; the wait must not
    /// run its full FRAME_WAIT then.
    #[test]
    fn a_wake_before_the_wait_ends_it_at_once() {
        let frames = FrameMailbox::default();
        frames.wake();
        let started = Instant::now();
        assert!(frames.pop_timeout(FRAME_WAIT).is_none());
        let waited = started.elapsed();
        assert!(waited < FRAME_WAIT / 10, "waited {waited:?}");
        // The wake is used up: the next wait runs its course.
        let started = Instant::now();
        assert!(frames.pop_timeout(Duration::from_millis(20)).is_none());
        assert!(started.elapsed() >= Duration::from_millis(20));
    }
}
