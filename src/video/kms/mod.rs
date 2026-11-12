//! The Linux pipeline. Scanout frames go from the display engine into the
//! GPU's encoder (VAAPI, or NVENC through CUDA on NVIDIA's driver) without
//! touching a CPU core, and the worker pulls them itself: the streaming
//! thread submits nothing.

mod capture;
pub mod capture_test;

pub use capture::{h265_enabled, probe, Backend, CaptureTarget, KmsFailure, Probed};

use super::{
    annex_b::unit_has_parameter_sets,
    display::Display,
    telemetry::{FrameSample, StageWindow},
    within_idr_holdoff, Captured, Configured, EncoderRequest, FramePacer, Pipeline, QualityTarget,
    FIRST_FRAME_TIMEOUT,
};
use crate::protocol::{VideoCodec, VideoConfig};
use crate::session::worker::{FailureSlot, Wire, Worker, WorkerFailure};
use capture::{KmsEncoder, Pull};
use std::{
    error::Error,
    thread,
    time::{Duration, Instant},
};

/// How long a capture open waits for a scanout. The display is gone for
/// 2 seconds or more while the KVM re-plugs it with a new EDID.
const CAPTURE_OPEN_TIMEOUT: Duration = Duration::from_secs(8);
/// How often a capture open looks for the scanout again.
const SCANOUT_POLL: Duration = Duration::from_millis(250);
/// Reopens that an interrupted or stalled capture gets before video fails.
const REOPEN_ATTEMPTS: u8 = 3;
/// VAAPI forces IDRs in-stream on request, and the KVM asks for one when a
/// viewer connects and when frames are lost, so the stream schedules IDRs
/// as rarely as Intel's driver allows (its GOP field is 16 bits): about one
/// each 9 minutes at 120 fps. A scheduled IDR of a detailed desktop (about
/// 500 KB at 1080p) takes a KVM's USB link longer to send than a viewer waits
/// before it counts a freeze.
const GOP_FRAMES: i32 = u16::MAX as i32;
/// How long the worker waits for a command while no capture is live, before
/// it steps again to ping.
const IDLE_WAIT: Duration = Duration::from_millis(50);

/// The capture that is streaming, with the configuration it was announced
/// under.
struct Live {
    capture: KmsEncoder,
    config: VideoConfig,
}

/// What one pull wrote to the wire.
#[derive(Default)]
struct Written {
    bytes: usize,
    keyframe: bool,
    elapsed: Duration,
    failed: bool,
}

pub struct KmsPipeline {
    fault: FailureSlot,
    target: Option<CaptureTarget>,
    live: Option<Live>,
    default_kbps: u32,
    codec: VideoCodec,
    quality: QualityTarget,
    /// The next pull keys its frame.
    force_idr: bool,
    reopen_attempts: u8,
    /// When a capture that has produced no frame counts as stalled.
    frame_deadline: Option<Instant>,
    last_idr: Option<Instant>,
    stages: StageWindow,
}

impl KmsPipeline {
    pub fn new(fault: FailureSlot) -> Self {
        Self {
            fault,
            target: None,
            live: None,
            default_kbps: 0,
            codec: VideoCodec::H264,
            quality: QualityTarget::default(),
            force_idr: false,
            reopen_attempts: 0,
            frame_deadline: None,
            last_idr: None,
            stages: StageWindow::new(),
        }
    }

    /// Announces the stream a new capture produces and makes it the live
    /// one, with the recovery budget reset.
    fn start(
        &mut self,
        capture: KmsEncoder,
        config: VideoConfig,
        quality: QualityTarget,
        wire: &mut Wire,
    ) -> Result<(), WorkerFailure> {
        self.quality = quality;
        self.announce(&capture, config, wire)?;
        self.live = Some(Live { capture, config });
        self.force_idr = false;
        self.reopen_attempts = 0;
        self.expect_frame();
        Ok(())
    }

    fn announce(
        &self,
        capture: &KmsEncoder,
        config: VideoConfig,
        wire: &mut Wire,
    ) -> Result<(), WorkerFailure> {
        let dims = (capture.width as usize, capture.height as usize);
        wire.announce_stream(
            config,
            self.codec,
            dims,
            self.default_kbps,
            self.quality,
            true,
        )
    }

    fn expect_frame(&mut self) {
        self.frame_deadline = Some(Instant::now() + FIRST_FRAME_TIMEOUT);
    }

    fn stalled(&self) -> bool {
        self.frame_deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
    }

    /// Opens a capture on the live scanout.
    fn open_capture(
        &self,
        quality: QualityTarget,
        codec: VideoCodec,
        wire: &mut Wire,
    ) -> Result<KmsEncoder, KmsFailure> {
        let target = self.target.as_ref().expect("configure sets the target");
        let fps = quality.effective_fps();
        wait_for_scanout(wire, || {
            KmsEncoder::new(
                target.backend,
                &target.device,
                target.connector,
                codec,
                fps as i32,
                quality.effective_kbps(self.default_kbps) as i32,
                GOP_FRAMES,
            )
        })
    }

    /// Retargets the live capture in place. False when there is none, or it
    /// cannot change its rate.
    fn retarget_live(
        &mut self,
        config: VideoConfig,
        quality: QualityTarget,
        wire: &mut Wire,
    ) -> Result<bool, WorkerFailure> {
        let Some(mut live) = self.live.take() else {
            return Ok(false);
        };
        let dims = (live.capture.width as usize, live.capture.height as usize);
        let set_rate = |fps, kbps| live.capture.set_rate(fps, kbps);
        let kept = self.retarget(set_rate, config, quality, dims, wire);
        if matches!(kept, Ok(true)) {
            live.config = config;
        }
        self.live = Some(live);
        kept
    }

    /// Gives `quality` to the running encoder through `set_rate`, which takes
    /// the frame rate and the bitrate. When the encoder accepts, the stream
    /// goes on, and the status that reports the target keeps it. False, with
    /// nothing written, when the encoder refuses.
    fn retarget(
        &mut self,
        set_rate: impl FnOnce(i32, i32) -> bool,
        config: VideoConfig,
        quality: QualityTarget,
        dims: (usize, usize),
        wire: &mut Wire,
    ) -> Result<bool, WorkerFailure> {
        let fps = quality.effective_fps() as i32;
        let kbps = quality.effective_kbps(self.default_kbps);
        if !set_rate(fps, kbps as i32) {
            eprintln!("kms encoder cannot change its rate in place; recreating it");
            return Ok(false);
        }
        eprintln!("kms encoder changed its rate in place: {kbps} kbps at {fps} fps");
        self.quality = quality;
        wire.announce_stream(config, self.codec, dims, self.default_kbps, quality, false)?;
        Ok(true)
    }

    /// Pulls one frame and writes its access units.
    fn pull(&mut self, live: &mut Live, wire: &mut Wire) -> (Pull, Written) {
        let codec = self.codec;
        let mut written = Written::default();
        let pull = live.capture.next(self.force_idr, |unit, key, pts_us| {
            let started = Instant::now();
            let parameter_sets = unit_has_parameter_sets(codec, unit);
            match wire.write_access_unit(unit, pts_us, key, parameter_sets) {
                Ok(()) => {
                    written.bytes += unit.len();
                    written.keyframe |= key;
                }
                Err(_) => written.failed = true,
            }
            written.elapsed += started.elapsed();
        });
        (pull, written)
    }

    fn record_frame(&mut self, written: &Written, encode: Duration) {
        self.reopen_attempts = 0;
        self.expect_frame();
        if written.keyframe {
            self.last_idr = Some(Instant::now());
            self.force_idr = false;
        }
        self.stages.record_frame(FrameSample {
            capture_wait: Duration::ZERO,
            convert: Duration::ZERO,
            encode,
            write: written.elapsed,
            bytes: written.bytes,
            keyframe: written.keyframe,
            fresh: true,
        });
    }

    /// Rebuilds the capture. A new display configuration needs a new capture;
    /// that is routine and spends none of the recovery budget. A failed grab,
    /// or a capture that stops producing frames, is reopened within one
    /// bounded budget.
    fn reopen(&mut self, routine: bool, stalled: bool, wire: &mut Wire) {
        if !routine && !self.spend_reopen_attempt(stalled) {
            return;
        }
        let Some(Live { capture, config }) = self.live.take() else {
            return;
        };
        drop(capture);
        thread::sleep(Duration::from_millis(100));
        let capture = match self.open_capture(self.quality, self.codec, wire) {
            Ok(capture) => capture,
            Err(err) => {
                self.fault
                    .set(format!("could not reopen KMS capture: {err}"));
                return;
            }
        };
        if self.announce(&capture, config, wire).is_err() {
            return;
        }
        self.live = Some(Live { capture, config });
        self.force_idr = true;
        // A routine reopen spends no attempt and buys no time: the frame
        // deadline runs on from the last real progress, so a scanout that
        // keeps changing without a frame still reaches the bounded recovery.
        if !routine || self.frame_deadline.is_none() {
            self.expect_frame();
        }
    }

    /// Counts a recovery reopen; false, with video failed, when the budget
    /// is spent.
    fn spend_reopen_attempt(&mut self, stalled: bool) -> bool {
        if self.reopen_attempts >= REOPEN_ATTEMPTS {
            let message = if stalled {
                format!(
                    "kms capture produced no frame within {FIRST_FRAME_TIMEOUT:?} after {REOPEN_ATTEMPTS} reopen attempts"
                )
            } else {
                format!("kms capture grab failed after {REOPEN_ATTEMPTS} reopen attempts")
            };
            self.live = None;
            self.fault.set(message);
            return false;
        }
        self.reopen_attempts += 1;
        eprintln!(
            "kms capture {}; reopening (attempt {}/{REOPEN_ATTEMPTS})",
            if stalled { "stalled" } else { "interrupted" },
            self.reopen_attempts,
        );
        true
    }
}

impl Pipeline for KmsPipeline {
    fn configure(
        &mut self,
        request: EncoderRequest,
        wire: &mut Wire,
    ) -> Result<Configured, WorkerFailure> {
        let target = CaptureTarget::for_display(&request.display)
            .map_err(|err| self.fault.record(err.to_string()))?;
        let backend = target.backend;
        self.target = Some(target);
        let default_kbps =
            |codec| crate::video::bitrate::default_kbps(codec, request.width, request.height);
        let quality = QualityTarget::from_config(request.config);
        self.codec = request.config.codec;
        self.default_kbps = default_kbps(self.codec);
        let mut opened = self.open_capture(quality, self.codec, wire);
        // Only the encoder itself refusing to open says anything about the
        // codec, and H.264 is the answer to it.
        if opened.as_ref().err() == Some(&KmsFailure::EncoderUnavailable)
            && self.codec == VideoCodec::H265
        {
            eprintln!(
                "{} unavailable; falling back to H.264",
                backend.encoder(VideoCodec::H265)
            );
            self.codec = VideoCodec::H264;
            self.default_kbps = default_kbps(self.codec);
            opened = self.open_capture(quality, self.codec, wire);
        }
        let capture = opened.map_err(|err| self.fault.record(err.to_string()))?;
        self.start(capture, request.config, quality, wire)?;
        Ok(Configured {
            bitrate: quality.effective_kbps(self.default_kbps),
        })
    }

    /// The running encoder takes a new target in place and keeps the stream.
    /// When it cannot, a new capture replaces it and starts a new stream.
    fn apply_quality(
        &mut self,
        config: VideoConfig,
        wire: &mut Wire,
    ) -> Result<bool, WorkerFailure> {
        let quality = QualityTarget::from_config(config);
        if self.retarget_live(config, quality, wire)? {
            return Ok(false);
        }
        let capture = self
            .open_capture(quality, self.codec, wire)
            .map_err(|err| self.fault.record(err.to_string()))?;
        self.start(capture, config, quality, wire)?;
        Ok(true)
    }

    /// Drops the capture, and with it the scanout, before the KVM pulls the
    /// display.
    fn stop(&mut self) {
        self.live = None;
        self.frame_deadline = None;
    }

    fn keyframe_request(&mut self) {
        if !within_idr_holdoff(self.last_idr) {
            self.force_idr = true;
        }
    }

    /// One pull, with the liveness ping first: a pull can block for a frame
    /// interval.
    fn step(&mut self, wire: &mut Wire, _commands_left: bool) {
        wire.ping_if_due();
        if wire.failed() {
            thread::sleep(Duration::from_millis(100));
            return;
        }
        let Some(mut live) = self.live.take() else {
            return;
        };
        let started = Instant::now();
        let (pull, written) = self.pull(&mut live, wire);
        self.live = Some(live);
        if written.failed {
            return;
        }
        let progress = pull == Pull::Encoded && written.bytes > 0;
        let stalled = !progress && self.stalled();
        match pull {
            Pull::ScanoutChanged => {
                if !stalled {
                    eprintln!("kms scanout changed; reopening");
                }
                self.reopen(!stalled, stalled, wire)
            }
            Pull::RateRejected => {
                eprintln!("kms encoder refused its new rate; recreating it");
                self.reopen(true, false, wire)
            }
            Pull::GrabFailed => self.reopen(false, stalled, wire),
            _ if stalled => self.reopen(false, true, wire),
            Pull::Encoded if progress => {
                self.record_frame(&written, started.elapsed() - written.elapsed)
            }
            Pull::Encoded => {}
            Pull::Idle => self.stages.add_idle(1),
            Pull::Failed(code) => {
                self.live = None;
                self.fault.set(format!("kms capture/encode failed: {code}"));
            }
        }
        self.stages.maybe_report();
    }

    /// With no capture there is nothing to pull: the worker waits on its
    /// commands instead.
    fn command_wait(&self) -> Duration {
        if self.live.is_none() {
            IDLE_WAIT
        } else {
            Duration::ZERO
        }
    }
}

/// Runs `open` until it finds a scanout. The scanout is briefly absent while
/// the KVM re-plugs the display, so that failure is waited out, with the
/// service replies still written; every other failure is returned at once.
fn wait_for_scanout<T>(
    wire: &mut Wire,
    mut open: impl FnMut() -> Result<T, KmsFailure>,
) -> Result<T, KmsFailure> {
    let deadline = Instant::now() + CAPTURE_OPEN_TIMEOUT;
    loop {
        wire.ping_if_due();
        let attempt = open();
        if !matches!(attempt, Err(KmsFailure::NoScanout)) || Instant::now() >= deadline {
            return attempt;
        }
        wire.wait_serving_replies(SCANOUT_POLL);
    }
}

/// The streaming thread's side of KMS capture. The worker pulls frames from
/// the scanout itself, so the thread only paces its control-record handling.
pub struct Source {
    pacer: FramePacer,
}

impl Source {
    pub fn open(_display: Display, _configured: &Configured) -> Result<Self, Box<dyn Error>> {
        Ok(Self {
            pacer: FramePacer::new(),
        })
    }

    pub fn next(&mut self, _worker: &Worker) -> Result<Captured, Box<dyn Error>> {
        self.pacer.wait();
        Ok(Captured::Idle)
    }

    pub fn reopen(&mut self, _display: Display) -> Result<(), Box<dyn Error>> {
        Ok(())
    }

    pub fn close(&mut self) {}

    pub fn set_configured(&mut self, _configured: &Configured) {}

    pub fn keyframe_requested(&mut self) {}

    pub fn stream_reset(&mut self) -> Result<(), Box<dyn Error>> {
        Ok(())
    }

    pub fn needs_new_capturer(&self) -> bool {
        false
    }

    pub fn renew(&mut self, _display: Display) -> Result<(), Box<dyn Error>> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{
        codec::{self, Message},
        ClipboardReply, ClipboardStatus, DisplayLayout, ServiceReply, Transmit, VideoStatus,
    };
    use std::sync::{atomic::AtomicBool, mpsc, Arc};

    /// The OUT endpoint as the test sees it: each message written.
    struct Recorder {
        written: mpsc::Sender<Message>,
        cancelled: Arc<AtomicBool>,
    }

    impl Transmit for Recorder {
        fn cancellation(&self) -> &Arc<AtomicBool> {
            &self.cancelled
        }

        fn send(&mut self, message: &Message) -> Result<(), Box<dyn Error>> {
            let _ = self.written.send(message.clone());
            Ok(())
        }

        fn send_frame(&mut self, _: &[u8], _: u64, _: bool, _: bool) -> Result<(), Box<dyn Error>> {
            Ok(())
        }
    }

    /// The KVM abandons a clipboard request after 2 seconds, and a display it
    /// re-plugs has no scanout for 2 seconds or more. An answer queued while
    /// the capture finds no scanout goes out before the next attempt.
    #[test]
    fn a_reply_is_written_while_the_capture_waits_for_a_scanout() {
        let (written, wire_log) = mpsc::channel();
        let (mut wire, replier) = Wire::with_replier(Recorder {
            written,
            cancelled: Arc::default(),
        });
        let answer = ServiceReply::Clipboard(ClipboardReply::Status {
            request_id: 7,
            status: ClipboardStatus::Ok,
        });
        let mut attempts = 0;
        let opened = wait_for_scanout(&mut wire, || {
            attempts += 1;
            if attempts == 1 {
                replier.reply(answer.clone());
                return Err(KmsFailure::NoScanout);
            }
            // The second attempt finds the scanout; the wire holds what was
            // written while the capture waited for it.
            Ok(wire_log.try_iter().collect::<Vec<_>>())
        });
        assert_eq!(opened, Ok(vec![codec::service_reply(&answer).unwrap()]));
    }

    fn recording_wire() -> (Wire, mpsc::Receiver<Message>) {
        let (written, wire_log) = mpsc::channel();
        let (wire, _) = Wire::with_replier(Recorder {
            written,
            cancelled: Arc::default(),
        });
        (wire, wire_log)
    }

    fn quality_config(target_kbps: u32, maximum_fps: u32) -> VideoConfig {
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

    /// The KVM follows the network with new targets while video plays. An
    /// encoder that takes the new rate keeps the stream, so the status that
    /// reports the target must not start a new one: the KVM would wait for
    /// a sync IDR and the viewer would see a freeze.
    #[test]
    fn a_target_the_encoder_accepts_keeps_the_stream() {
        let (mut wire, wire_log) = recording_wire();
        let mut pipeline = KmsPipeline::new(FailureSlot::default());
        pipeline.default_kbps = 9_000;
        let config = quality_config(30_000, 60);
        let mut given = None;
        let kept = pipeline.retarget(
            |fps, kbps| {
                given = Some((fps, kbps));
                true
            },
            config,
            QualityTarget::from_config(config),
            (1920, 1080),
            &mut wire,
        );
        assert_eq!(kept.ok(), Some(true));
        // The bitrate is capped at twice the default, as at open.
        assert_eq!(given, Some((60, 18_000)));
        let status = VideoStatus {
            enabled: true,
            layout: DisplayLayout::Mirror,
            codec: VideoCodec::H264,
            stream_reset: false,
            actual_width: 1920,
            actual_height: 1080,
            native_fps: 120,
            default_kbps: 9_000,
            target_kbps: 18_000,
            maximum_fps: 60,
            error_code: 0,
        };
        assert_eq!(
            wire_log.try_iter().collect::<Vec<_>>(),
            vec![codec::video_status(&status).unwrap()]
        );
    }

    /// A refusal leaves the stream to the replacement encoder, which
    /// announces it; this path writes nothing and keeps the old target.
    #[test]
    fn a_refused_target_writes_nothing() {
        let (mut wire, wire_log) = recording_wire();
        let mut pipeline = KmsPipeline::new(FailureSlot::default());
        pipeline.default_kbps = 9_000;
        let config = quality_config(4_000, 0);
        let kept = pipeline.retarget(
            |_, _| false,
            config,
            QualityTarget::from_config(config),
            (1920, 1080),
            &mut wire,
        );
        assert_eq!(kept.ok(), Some(false));
        assert_eq!(pipeline.quality, QualityTarget::default());
        assert!(wire_log.try_iter().next().is_none());
    }
}
