//! The OUT endpoint side of a session. One thread owns the endpoint: it
//! writes registrations, service replies, frames and liveness pings, and
//! closes the session at the end. Frames come from the platform's video
//! pipeline, which the thread drives between command passes. The rest of
//! the process reaches the wire through `Worker`.

use super::control::ControlLink;
use super::CONTROL_POLL;
use crate::protocol::{
    codec, CloseReason, DisplayLayout, Registration, ServiceReply, TerminalReply, Transmit,
    VideoCodec, VideoConfig, VideoStatus, VIDEO_DISPLAY_UNAVAILABLE,
};
#[cfg(not(target_os = "linux"))]
use crate::video::ram::{EncoderCodecs, FrameJob, FrameMailbox};
use crate::video::{Configured, EncoderRequest, Pipeline, PlatformPipeline, QualityTarget, FPS};
use std::{
    collections::VecDeque,
    error::Error,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc, Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

/// A static desktop produces no frames, and a killed sender produces no USB
/// event on the KVM. The ping keeps records flowing, so the KVM can treat a
/// long silence as a dead sender and restore HDMI capture.
const PING_INTERVAL: Duration = Duration::from_secs(2);
/// Shell output taken from the shells per control pass, or queued for the
/// worker but not yet written. Past this the control thread stops draining
/// the shells, which then block on their own writes, so a fast printer
/// cannot outrun USB or starve the control records.
const TERMINAL_OUTPUT_IN_FLIGHT: usize = 256 << 10;
/// Service replies written per pass before the video pipeline gets its
/// turn, so a shell that refills the queue as fast as it drains still
/// shares the wire with frames.
const SERVICE_REPLIES_PER_PASS: usize = 8;
const CONFIGURE_TIMEOUT: Duration = Duration::from_secs(15);
const STOP_TIMEOUT: Duration = Duration::from_secs(5);

/// A failure the worker reports: the message the owner logs.
#[derive(Clone, Debug)]
pub struct WorkerFailure {
    message: String,
}

impl From<String> for WorkerFailure {
    fn from(message: String) -> Self {
        Self { message }
    }
}

impl From<&str> for WorkerFailure {
    fn from(message: &str) -> Self {
        message.to_owned().into()
    }
}

impl std::fmt::Display for WorkerFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl Error for WorkerFailure {}

/// A failure shared between the worker thread and its owner. The first
/// failure wins, so the root cause survives what follows from it.
#[derive(Clone, Default)]
pub struct FailureSlot(Arc<Mutex<Option<WorkerFailure>>>);

impl FailureSlot {
    /// Keeps `failure` unless one is already kept; the owner reports it.
    pub fn set(&self, failure: impl Into<WorkerFailure>) {
        let mut slot = self.0.lock().unwrap();
        if slot.is_none() {
            *slot = Some(failure.into());
        }
    }

    /// Keeps `failure` and returns it, for a command's reply.
    pub fn record(&self, failure: impl Into<WorkerFailure>) -> WorkerFailure {
        let failure = failure.into();
        self.set(failure.clone());
        failure
    }

    fn is_set(&self) -> bool {
        self.0.lock().unwrap().is_some()
    }

    pub fn get(&self) -> Option<WorkerFailure> {
        self.0.lock().unwrap().clone()
    }

    fn take(&self) -> Option<WorkerFailure> {
        self.0.lock().unwrap().take()
    }
}

type Reply<T> = mpsc::Sender<Result<T, WorkerFailure>>;

/// What the streaming thread asks the video pipeline to do.
enum VideoCommand {
    #[cfg(not(target_os = "linux"))]
    AttachCodecs(EncoderCodecs),
    /// Creates the encoder for a display and announces the stream.
    Configure {
        request: EncoderRequest,
        reply: Reply<Configured>,
    },
    /// Replies whether the stream was reset for the new target.
    ApplyQuality {
        config: VideoConfig,
        reply: Reply<bool>,
    },
    /// Drops the encoder and capture, then reports video disabled.
    Stop {
        config: VideoConfig,
        display_unavailable: bool,
        reply: Reply<()>,
    },
    KeyframeRequest,
}

enum Command {
    Register(Registration),
    Reply(ServiceReply),
    /// Sends session.close, then exits. Dropping the command sender instead
    /// exits without a close, and the KVM treats the session as failed.
    Close,
    Video(VideoCommand),
}

/// Whether the worker goes on after a command pass.
enum Pass {
    Continue,
    /// It served its share of replies and more commands are queued.
    Yield,
    Exit,
}

/// The OUT endpoint with what every write shares: the first wire failure
/// ends the session, and the time of the last write paces the pings.
pub struct Wire {
    tx: Box<dyn Transmit>,
    commands: mpsc::Receiver<Command>,
    /// Commands taken while a pipeline waited, for the next command pass.
    held: VecDeque<Command>,
    error: FailureSlot,
    last_write: Instant,
    queued_output: Arc<AtomicUsize>,
    /// The owner closes the session at once: nothing more goes out but
    /// session.close.
    closing: Arc<AtomicBool>,
}

impl Wire {
    /// Ends the session with this failure, logs it, and returns it for a
    /// reply.
    pub fn fail(&self, failure: impl Into<WorkerFailure>) -> WorkerFailure {
        let failure = failure.into();
        if !self.error.is_set() {
            eprintln!("USB writer failed: {failure}");
        }
        self.error.record(failure)
    }

    pub fn failed(&self) -> bool {
        self.error.is_set()
    }

    fn wrote(&mut self, result: Result<(), Box<dyn Error>>) -> Result<(), WorkerFailure> {
        match result {
            Ok(()) => {
                self.last_write = Instant::now();
                Ok(())
            }
            Err(err) => Err(self.fail(err.to_string())),
        }
    }

    fn send(&mut self, message: std::io::Result<codec::Message>) -> Result<(), WorkerFailure> {
        let written = message
            .map_err(Into::into)
            .and_then(|message| self.tx.send(&message));
        self.wrote(written)
    }

    pub fn write_access_unit(
        &mut self,
        unit: &[u8],
        capture_us: u64,
        keyframe: bool,
        parameter_sets: bool,
    ) -> Result<(), WorkerFailure> {
        let written = self
            .tx
            .send_frame(unit, capture_us, keyframe, parameter_sets);
        self.wrote(written)
    }

    /// Reports the applied stream. The status precedes the new encoder's
    /// first frame and starts a new stream epoch on the KVM.
    pub fn announce_stream(
        &mut self,
        config: VideoConfig,
        codec: VideoCodec,
        (width, height): (usize, usize),
        default_kbps: u32,
        quality: QualityTarget,
        stream_reset: bool,
    ) -> Result<(), WorkerFailure> {
        let status = VideoStatus {
            enabled: true,
            layout: config.layout,
            codec,
            stream_reset,
            actual_width: u32::try_from(width).map_err(|err| self.fail(err.to_string()))?,
            actual_height: u32::try_from(height).map_err(|err| self.fail(err.to_string()))?,
            native_fps: FPS as u32,
            default_kbps,
            target_kbps: quality.applied_kbps(default_kbps),
            maximum_fps: quality.applied_maximum_fps(),
            error_code: 0,
        };
        self.send(codec::video_status(&status))
    }

    fn write_disabled_status(
        &mut self,
        layout: DisplayLayout,
        codec: VideoCodec,
        display_unavailable: bool,
    ) -> Result<(), WorkerFailure> {
        let status = VideoStatus {
            enabled: false,
            layout,
            codec,
            stream_reset: true,
            actual_width: 0,
            actual_height: 0,
            native_fps: 0,
            default_kbps: 0,
            target_kbps: 0,
            maximum_fps: 0,
            error_code: if display_unavailable {
                VIDEO_DISPLAY_UNAVAILABLE
            } else {
                0
            },
        };
        self.send(codec::video_status(&status))
    }

    pub fn ping_if_due(&mut self) {
        if !self.failed() && self.last_write.elapsed() >= PING_INTERVAL {
            let _ = self.send(Ok(codec::ping()));
        }
    }

    /// Spends `wait` writing the service replies that arrive meanwhile. A
    /// pipeline that waits for the display calls this instead of sleeping:
    /// the display is gone for 2 seconds or more while the KVM re-plugs it,
    /// and the KVM abandons a clipboard request after 2 seconds. Other
    /// commands are kept, in order, for the next command pass.
    #[cfg(target_os = "linux")]
    pub fn wait_serving_replies(&mut self, wait: Duration) {
        let deadline = Instant::now() + wait;
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            match self.commands.recv_timeout(left) {
                Ok(Command::Reply(reply)) => self.write_reply(&reply),
                Ok(command) => self.held.push_back(command),
                Err(mpsc::RecvTimeoutError::Timeout) => return,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    thread::sleep(left);
                    return;
                }
            }
        }
    }

    fn next_command(&mut self, wait: Duration) -> Result<Command, mpsc::RecvTimeoutError> {
        match self.held.pop_front() {
            Some(command) => Ok(command),
            None => self.commands.recv_timeout(wait),
        }
    }

    /// Serves the queued commands, the video ones through the pipeline,
    /// first waiting for one as long as the pipeline lets it. Yields after
    /// SERVICE_REPLIES_PER_PASS replies so video gets its turn, and exits
    /// after the close or when the owner dropped its handle.
    fn serve_commands(&mut self, video: &mut PlatformPipeline) -> Pass {
        let mut replies = 0;
        let mut wait = video.command_wait();
        while !self.tx.is_cancelled() {
            if self.close_if_asked() {
                return Pass::Exit;
            }
            let command = match self.next_command(wait) {
                Ok(command) => command,
                Err(mpsc::RecvTimeoutError::Timeout) => return Pass::Continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => return Pass::Exit,
            };
            wait = Duration::ZERO;
            match command {
                Command::Close => {
                    self.close();
                    return Pass::Exit;
                }
                Command::Register(registration) => {
                    let _ = self.send(codec::registration(&registration));
                }
                Command::Reply(reply) => {
                    self.write_reply(&reply);
                    replies += 1;
                    if replies == SERVICE_REPLIES_PER_PASS {
                        return Pass::Yield;
                    }
                }
                Command::Video(command) => self.handle_video(command, video),
            }
        }
        Pass::Exit
    }

    fn close(&mut self) {
        if let Err(err) = self.tx.send(&codec::close(CloseReason::Shutdown)) {
            eprintln!("ipkvm-os-services: could not close session cleanly: {err}");
        }
    }

    /// Sends session.close when the owner closes the session at once, ahead
    /// of the commands that wait; true when it did.
    fn close_if_asked(&mut self) -> bool {
        let asked = self.closing.load(Ordering::Acquire);
        if asked {
            self.close();
        }
        asked
    }

    /// After a wire failure nothing more goes out, but the in-flight
    /// accounting still settles so the shells drain.
    fn write_reply(&mut self, reply: &ServiceReply) {
        if !self.failed() {
            let _ = self.send(codec::service_reply(reply));
        }
        self.queued_output
            .fetch_sub(terminal_output_len(reply), Ordering::Relaxed);
    }

    fn handle_video(&mut self, command: VideoCommand, video: &mut PlatformPipeline) {
        match command {
            #[cfg(not(target_os = "linux"))]
            VideoCommand::AttachCodecs(codecs) => video.attach(codecs),
            VideoCommand::Configure { request, reply } => {
                let _ = reply.send(video.configure(request, self));
            }
            VideoCommand::ApplyQuality { config, reply } => {
                let _ = reply.send(video.apply_quality(config, self));
            }
            VideoCommand::Stop {
                config,
                display_unavailable,
                reply,
            } => {
                video.stop();
                let written =
                    self.write_disabled_status(config.layout, config.codec, display_unavailable);
                let _ = reply.send(written);
            }
            VideoCommand::KeyframeRequest if self.failed() => {}
            VideoCommand::KeyframeRequest => video.keyframe_request(),
        }
    }
}

/// Shell output bytes a reply carries, for the in-flight accounting.
fn terminal_output_len(reply: &ServiceReply) -> usize {
    match reply {
        ServiceReply::Terminal(TerminalReply::Output { data, .. }) => data.len(),
        _ => 0,
    }
}

/// Answers a service message on the worker's wire, from any thread.
#[derive(Clone)]
pub struct ServiceReplier {
    commands: mpsc::Sender<Command>,
    #[cfg(not(target_os = "linux"))]
    frames: Arc<FrameMailbox>,
    queued_output: Arc<AtomicUsize>,
}

impl ServiceReplier {
    pub fn reply(&self, reply: ServiceReply) {
        self.queued_output
            .fetch_add(terminal_output_len(&reply), Ordering::Relaxed);
        self.send(Command::Reply(reply));
    }

    fn send(&self, command: Command) {
        let _ = self.commands.send(command);
        // The command is served now, not at the next frame: a keystroke's
        // echo on an idle desktop must not wait for one. The KMS worker
        // needs no wake: with no capture it waits on the commands.
        #[cfg(not(target_os = "linux"))]
        self.frames.wake();
    }
}

/// The owner's handle on the worker thread.
pub struct Worker {
    replier: ServiceReplier,
    /// A wire or worker failure: the session is over.
    error: FailureSlot,
    /// A capture or encode failure: video is over, the session is not. The
    /// session withdraws video on it and registers again once the capture
    /// probes pass.
    fault: FailureSlot,
    cancelled: Arc<AtomicBool>,
    closing: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl Worker {
    /// Starts the thread that owns the OUT endpoint. It serves
    /// registrations, replies and pings from the start; frames need codecs
    /// or a capture target, which come later.
    pub fn spawn(tx: impl Transmit + 'static) -> Self {
        let cancelled = tx.cancellation().clone();
        let (commands, command_rx) = mpsc::channel();
        let error = FailureSlot::default();
        let fault = FailureSlot::default();
        let queued_output = Arc::new(AtomicUsize::new(0));
        let closing = Arc::new(AtomicBool::new(false));
        #[cfg(not(target_os = "linux"))]
        let frames = Arc::new(FrameMailbox::default());
        let mut wire = Wire {
            tx: Box::new(tx),
            commands: command_rx,
            held: VecDeque::new(),
            error: error.clone(),
            last_write: Instant::now(),
            queued_output: queued_output.clone(),
            closing: closing.clone(),
        };
        let pipeline_fault = fault.clone();
        #[cfg(not(target_os = "linux"))]
        let pipeline_frames = frames.clone();
        let handle = thread::Builder::new()
            .name("usb-writer".into())
            .spawn(move || {
                // Encoders stay on the thread that opens them.
                #[cfg(target_os = "linux")]
                let mut video = PlatformPipeline::new(pipeline_fault);
                #[cfg(not(target_os = "linux"))]
                let mut video = PlatformPipeline::new(pipeline_frames, pipeline_fault);
                loop {
                    let commands_left = match wire.serve_commands(&mut video) {
                        Pass::Continue => false,
                        Pass::Yield => true,
                        Pass::Exit => break,
                    };
                    // A pass that yields to video has not looked at the
                    // commands that wait, and no frame may go out after
                    // the owner closes the session at once.
                    if wire.close_if_asked() {
                        break;
                    }
                    video.step(&mut wire, commands_left);
                }
            })
            .expect("spawn the USB writer");
        Self {
            replier: ServiceReplier {
                commands,
                #[cfg(not(target_os = "linux"))]
                frames,
                queued_output,
            },
            error,
            fault,
            cancelled,
            closing,
            handle: Some(handle),
        }
    }

    pub fn register(&self, registration: Registration) {
        self.replier.send(Command::Register(registration));
    }

    #[cfg(not(target_os = "linux"))]
    pub fn attach_codecs(&self, codecs: EncoderCodecs) {
        self.video(VideoCommand::AttachCodecs(codecs));
    }

    pub fn request_keyframe(&self) {
        self.video(VideoCommand::KeyframeRequest);
    }

    pub fn reply(&self, reply: ServiceReply) {
        self.replier.reply(reply);
    }

    pub fn replier(&self) -> ServiceReplier {
        self.replier.clone()
    }

    fn video(&self, command: VideoCommand) {
        self.replier.send(Command::Video(command));
    }

    /// Sends a video command and waits for its reply. Opening an encoder
    /// can take seconds, so the session is served meanwhile: the KVM's
    /// control writes would otherwise block on an unread endpoint.
    fn request<T>(
        &self,
        command: impl FnOnce(Reply<T>) -> VideoCommand,
        timeout: Duration,
        link: &mut ControlLink,
    ) -> Result<T, Box<dyn Error>> {
        let (reply, answer) = mpsc::channel();
        self.video(command(reply));
        let deadline = Instant::now() + timeout;
        loop {
            match answer.recv_timeout(CONTROL_POLL) {
                Ok(result) => return Ok(result?),
                Err(mpsc::RecvTimeoutError::Timeout) if Instant::now() < deadline => {
                    link.serve(self)?
                }
                Err(_) => {
                    let failure = self.failure();
                    return Err(failure
                        .unwrap_or_else(|| "the USB writer did not answer".into())
                        .into());
                }
            }
        }
    }

    /// Creates the encoder for a display and announces the stream.
    pub fn configure(
        &self,
        request: EncoderRequest,
        link: &mut ControlLink,
    ) -> Result<Configured, Box<dyn Error>> {
        self.request(
            |reply| VideoCommand::Configure { request, reply },
            CONFIGURE_TIMEOUT,
            link,
        )
    }

    /// Stops capture and reports video disabled; `display_unavailable` marks
    /// a display that is asleep.
    pub fn stop_capture(
        &self,
        config: VideoConfig,
        display_unavailable: bool,
        link: &mut ControlLink,
    ) -> Result<(), Box<dyn Error>> {
        self.request(
            |reply| VideoCommand::Stop {
                config,
                display_unavailable,
                reply,
            },
            STOP_TIMEOUT,
            link,
        )
    }

    /// Applies a new quality target; true when the stream was reset.
    pub fn apply_quality(
        &self,
        config: VideoConfig,
        link: &mut ControlLink,
    ) -> Result<bool, Box<dyn Error>> {
        self.request(
            |reply| VideoCommand::ApplyQuality { config, reply },
            CONFIGURE_TIMEOUT,
            link,
        )
    }

    #[cfg(not(target_os = "linux"))]
    pub fn take_buffer(&self) -> Vec<u8> {
        self.replier.frames.take_buffer()
    }

    #[cfg(not(target_os = "linux"))]
    pub fn submit(&self, job: FrameJob) {
        self.replier.frames.push(job);
    }

    #[cfg(not(target_os = "linux"))]
    pub fn note_idle(&self) {
        self.replier.frames.note_idle();
    }

    /// How much more shell output the control thread may hand over now.
    pub fn terminal_output_budget(&self) -> usize {
        let queued = self.replier.queued_output.load(Ordering::Relaxed);
        TERMINAL_OUTPUT_IN_FLIGHT.saturating_sub(queued)
    }

    pub fn failure(&self) -> Option<WorkerFailure> {
        self.error.get()
    }

    /// The capture or encode failure that ended video, taken once.
    pub fn take_fault(&self) -> Option<WorkerFailure> {
        self.fault.take()
    }

    /// Closes the session cleanly while the reader still drains replies.
    pub fn stop(&mut self) {
        self.replier.send(Command::Close);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }

    /// Closes the session at once, for a KVM that must get nothing more:
    /// the writer sends session.close ahead of the replies that wait, and
    /// captures and sends no further frame.
    pub fn stop_now(&mut self) {
        self.closing.store(true, Ordering::Release);
        #[cfg(not(target_os = "linux"))]
        self.replier.frames.wake();
        self.stop();
    }

    pub fn cancel_and_join(&mut self) {
        self.cancelled.store(true, Ordering::Release);
        #[cfg(not(target_os = "linux"))]
        self.replier.frames.wake();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.cancel_and_join();
    }
}

#[cfg(target_os = "linux")]
impl Wire {
    /// A wire on `tx` with no worker thread, and the replier that feeds it.
    /// The tests and the KMS capture test drive a pipeline on it.
    pub fn with_replier(tx: impl Transmit + 'static) -> (Self, ServiceReplier) {
        let (commands, command_rx) = mpsc::channel();
        let queued_output = Arc::new(AtomicUsize::new(0));
        let closing = Arc::new(AtomicBool::new(false));
        let wire = Wire {
            tx: Box::new(tx),
            commands: command_rx,
            held: VecDeque::new(),
            error: FailureSlot::default(),
            last_write: Instant::now(),
            queued_output: queued_output.clone(),
            closing: closing.clone(),
        };
        let replier = ServiceReplier {
            commands,
            queued_output,
        };
        (wire, replier)
    }
}
