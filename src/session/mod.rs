//! One USB session with the KVM. The session opens with a reset and a hello,
//! registers the services that need no capture, and registers video whenever
//! the capture probes pass. A video failure withdraws video and keeps the
//! session; only a wire failure ends it.
//!
//! Two threads own the link: the worker writes the OUT endpoint
//! (`worker`), and the control reader drains the IN endpoint (`control`).
//! The thread that runs the session streams video and serves the records.

pub mod control;
pub mod worker;

use crate::approval::{Approval, PinStore};
use crate::protocol::{self, ControlRecord, Registration, VideoConfig};
use crate::services::{clipboard, file_transfer, update};
use crate::supervisor::update::{Installation, Staged};
use crate::video::{self, display, stream, CaptureFailure};
use control::{ControlLink, ControlReader};
use std::{
    error::Error,
    sync::atomic::AtomicBool,
    time::{Duration, Instant},
};
use worker::Worker;

/// How long a thread serving the session waits for a record before it
/// checks the session again; a record ends the wait at once.
pub const CONTROL_POLL: Duration = Duration::from_millis(50);
/// How often the capture probes run while video is unregistered.
const CAPTURE_PROBE_INTERVAL: Duration = Duration::from_secs(2);
/// How long video stays withdrawn after a capture or encode failure. A
/// display that does not come back, or an encoder that does not open, must
/// not make the KVM re-plug its display at every probe.
const VIDEO_RETRY: Duration = Duration::from_secs(10);
/// How long the KVM has to answer a video registration with video.config.
const REGISTRATION_TIMEOUT: Duration = Duration::from_secs(15);

/// Both USB threads. Teardown completes before `run` returns, so the next
/// session cannot claim the interface while this one still holds it.
struct Session<'a> {
    worker: Worker,
    link: ControlLink<'a>,
}

impl<'a> Session<'a> {
    fn open(
        shutdown: &'a AtomicBool,
        installation: Option<&Installation>,
    ) -> Result<(Self, String), Box<dyn Error>> {
        let store = PinStore::for_user()?;
        let negotiated = protocol::find_and_negotiate(&store.load()?)?;
        let approval = Approval::of(store, negotiated.kvm);
        let worker = Worker::spawn(negotiated.tx);
        let settings = negotiated.settings;
        let replier = worker.replier();
        let files = file_transfer::Files::new(settings.files_enabled, move |reply| {
            replier.reply(protocol::ServiceReply::File(reply))
        });
        let replier = worker.replier();
        let staged = update::StagedUpdate::default();
        let updates = update::Updates::new(installation.cloned(), staged.clone(), move |reply| {
            replier.reply(protocol::ServiceReply::Update(reply))
        });
        let reader = ControlReader::spawn(negotiated.rx, files, updates);
        let link = ControlLink::new(reader, &settings, shutdown, staged, approval);
        Ok((Self { worker, link }, negotiated.device))
    }
}

impl Drop for Session<'_> {
    fn drop(&mut self) {
        self.worker.cancel_and_join();
        self.link.stop_reader();
    }
}

/// What the capture probes found.
struct CaptureBackend {
    #[cfg(not(target_os = "linux"))]
    codecs: video::ram::EncoderCodecs,
    capabilities: protocol::VideoCapabilities,
    /// The H.264 encoder's name, for the log.
    encoder_label: String,
}

/// A display asleep when video last stopped: the display the user picked,
/// if any, and the display's name.
type PausedDisplay = (Option<String>, String);

/// Runs one session until the KVM or the user ends it. Ok means a clean
/// stop, or a staged update package to install.
pub fn run(
    shutdown: &AtomicBool,
    installation: Option<&Installation>,
) -> Result<Option<Staged>, Box<dyn Error>> {
    #[cfg(windows)]
    raise_thread_priority()?;
    let startup = Instant::now();
    let (mut session, device) = Session::open(shutdown, installation)?;
    let Session { worker, link } = &mut session;
    log_startup(&startup, "usb_ready");
    register_services(worker, link, installation);
    let served = serve(&startup, &device, worker, link);
    // A staged package installs however the session ended: the KVM has its
    // Installing status and waits for the new version.
    match (link.take_staged(), served) {
        (Some(staged), Err(err)) => {
            eprintln!("ipkvm-os-services: {err}");
            Ok(Some(staged))
        }
        (staged, served) => served.map(|()| staged),
    }
}

/// Serves the session until it ends, then closes it cleanly. Err means a
/// wire failure. A session that ends because the user removed its KVM from
/// the approved list closes cleanly too, and the next handshake refuses the
/// KVM. Its services end before the close: the close waits for the KVM to
/// read it, and a KVM that does not read must have no access meanwhile. The
/// close goes out ahead of the replies that wait, and no frame follows.
fn serve(
    startup: &Instant,
    device: &str,
    worker: &mut Worker,
    link: &mut ControlLink,
) -> Result<(), Box<dyn Error>> {
    match serve_video(startup, device, worker, link) {
        Err(err) if !err.is::<protocol::NotApproved>() => return Err(err),
        _ => {}
    }
    if link.approval_withdrawn() {
        link.end_services();
        worker.stop_now();
    } else {
        worker.stop();
    }
    Ok(())
}

/// Probes for capture and streams video until the session is asked to stop.
fn serve_video(
    startup: &Instant,
    device: &str,
    worker: &mut Worker,
    link: &mut ControlLink,
) -> Result<(), Box<dyn Error>> {
    let mut first_probe = Duration::ZERO;
    let mut paused: Option<PausedDisplay> = None;
    loop {
        let Some(found) = wait_for_capture(first_probe, paused.as_ref(), worker, link)? else {
            break;
        };
        let mut selector = display::DisplaySelector::new(link.capture_display());
        #[cfg(not(target_os = "linux"))]
        worker.attach_codecs(found.codecs);
        worker.register(Registration::Video(protocol::VideoCapabilities {
            sets_display_mode: link.sets_display_mode(),
            ..found.capabilities
        }));
        let Some(config) = wait_for_video_config(worker, link)? else {
            break;
        };
        log_startup(startup, "video_registered");
        let streamed = stream::stream_video(
            startup,
            device,
            &found.encoder_label,
            &mut selector,
            config,
            worker,
            link,
        );
        match streamed {
            Ok(()) => break,
            Err(err) if worker.failure().is_some() => return Err(err),
            Err(err) => {
                paused = withdraw_video(err.as_ref(), &selector, config, worker, link)?;
                first_probe = VIDEO_RETRY;
            }
        }
    }
    Ok(())
}

/// Clipboard, terminal, file transfer and updates need no capture and
/// register at once with the policy the session opened with; a later
/// session.config re-registers what it changes. An installation that cannot
/// replace itself offers no updates.
fn register_services(worker: &Worker, link: &ControlLink, installation: Option<&Installation>) {
    worker.register(Registration::Clipboard(clipboard::capabilities()));
    worker.register(Registration::Terminal {
        shell: link.terminal_enabled(),
    });
    worker.register(Registration::File(file_transfer::capabilities(
        link.files_allowed(),
    )));
    if let Some(installation) = installation {
        worker.register(Registration::Update(installation.package()));
    }
}

/// Stops capture and withdraws video after a video failure, keeping the
/// session. A display that is asleep pauses the probes until it wakes
/// instead of spending the retry budget; returns that display.
fn withdraw_video(
    err: &(dyn Error + 'static),
    selector: &display::DisplaySelector,
    config: VideoConfig,
    worker: &Worker,
    link: &mut ControlLink,
) -> Result<Option<PausedDisplay>, Box<dyn Error>> {
    let asleep = selector.inactive_display().unwrap_or(None);
    match &asleep {
        Some(name) => eprintln!("video paused: display {name} is inactive; waiting for wake"),
        None => eprintln!("ipkvm-os-services: video stopped: {err}"),
    }
    worker.stop_capture(config, asleep.is_some(), link)?;
    worker.register(Registration::VideoWithdrawn);
    // A fault recorded for this failure must not end the next registration.
    let _ = worker.take_fault();
    Ok(asleep.map(|name| (link.capture_display(), name)))
}

/// Serves the session until the capture probes pass; None when the session
/// is asked to stop. The probes run every CAPTURE_PROBE_INTERVAL, and only
/// while the display they need is present.
fn wait_for_capture(
    first_probe: Duration,
    paused: Option<&PausedDisplay>,
    worker: &Worker,
    link: &mut ControlLink,
) -> Result<Option<CaptureBackend>, Box<dyn Error>> {
    let mut next_probe = Instant::now() + first_probe;
    let mut last_reason = None;
    let mut probe = Probe::default();
    loop {
        if link.shutdown_requested() {
            probe.join();
            return Ok(None);
        }
        if let Some(failure) = worker.failure() {
            probe.join();
            return Err(failure.into());
        }
        if let Err(err) = link.next(worker, CONTROL_POLL) {
            probe.join();
            return Err(err);
        }
        let result = match probe.result() {
            Some(result) => Some(result),
            None if probe.running() || Instant::now() < next_probe => continue,
            None if !capture_display_present(paused, link)? => None,
            None => match capture_allowed() {
                Ok(()) => {
                    probe.start();
                    continue;
                }
                Err(reason) => Some(Err(reason)),
            },
        };
        match result {
            Some(Ok(found)) => return Ok(Some(found)),
            Some(Err(reason)) if last_reason != Some(reason) => {
                eprintln!("ipkvm-os-services: video not available: {reason}");
                #[cfg(target_os = "linux")]
                if reason == CaptureFailure::Kms(video::kms::KmsFailure::EncoderUnavailable) {
                    video::missing_encoder::tell_once();
                }
                last_reason = Some(reason);
            }
            _ => {}
        }
        next_probe = Instant::now() + CAPTURE_PROBE_INTERVAL;
    }
}

/// A capture probe on its own thread. An encoder probe takes seconds (2.2 s
/// with AMF on Windows), and the session thread must keep answering the
/// KVM meanwhile: the KVM gives a clipboard request 2 s.
#[derive(Default)]
struct Probe(Option<std::thread::JoinHandle<Result<CaptureBackend, CaptureFailure>>>);

impl Probe {
    fn start(&mut self) {
        let thread = std::thread::Builder::new()
            .name("capture-probe".into())
            .spawn(probe_capture)
            .expect("spawn capture probe thread");
        self.0 = Some(thread);
    }

    fn running(&self) -> bool {
        self.0.is_some()
    }

    /// The result of a probe that has finished. A probe that panicked
    /// reports the failure that asks for the log.
    fn result(&mut self) -> Option<Result<CaptureBackend, CaptureFailure>> {
        if !self.0.as_ref()?.is_finished() {
            return None;
        }
        let thread = self.0.take()?;
        Some(thread.join().unwrap_or(Err(PROBE_PANICKED)))
    }

    /// Waits for a probe that still runs, so that the next probe never runs
    /// alongside it.
    fn join(&mut self) {
        if let Some(thread) = self.0.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(target_os = "linux")]
const PROBE_PANICKED: CaptureFailure = CaptureFailure::Kms(video::kms::KmsFailure::PipelineFailed);
#[cfg(not(target_os = "linux"))]
const PROBE_PANICKED: CaptureFailure = CaptureFailure::EncoderUnavailable;

/// Whether this build may capture the screen. macOS keys the grant to the
/// code signature, and a capture opened without it fails at its first
/// frame, so the grant is checked before a probe, on the session thread,
/// which may show the system's request.
fn capture_allowed() -> Result<(), CaptureFailure> {
    #[cfg(target_os = "linux")]
    if crate::supervisor::linux::helper() == crate::supervisor::linux::Helper::Older {
        return Err(CaptureFailure::HelperOlder);
    }
    #[cfg(target_os = "macos")]
    if !crate::macos::screen_recording::granted() {
        crate::macos::screen_recording::request_once();
        return Err(CaptureFailure::ScreenRecordingNotGranted);
    }
    Ok(())
}

/// Whether the display capture would use is present and awake. Re-plugging
/// the KVM's display cannot make an absent display appear, so the probes
/// wait for it instead of disturbing the desktop.
fn capture_display_present(
    paused: Option<&PausedDisplay>,
    link: &ControlLink,
) -> Result<bool, Box<dyn Error>> {
    let requested = link.capture_display();
    let asleep = paused
        .filter(|(pick, _)| *pick == requested)
        .map(|(_, name)| name.clone());
    Ok(match requested.or(asleep) {
        Some(name) => display::find_online(&name)?.is_some(),
        None => display::DisplaySelector::new(None)
            .inactive_display()?
            .is_none(),
    })
}

/// The capture backend and the capabilities to register, or why there is
/// none yet. The answer changes while the session is open: a grant arrives,
/// a display comes back.
fn probe_capture() -> Result<CaptureBackend, CaptureFailure> {
    #[cfg(target_os = "linux")]
    {
        // The probe grabs a live scanout, which is absent while the KVM
        // re-plugs the display; a later probe finds it.
        let video::kms::Probed { card, encoder } =
            video::kms::probe().map_err(CaptureFailure::Kms)?;
        eprintln!("KMS capture available on {card} with {encoder}");
        Ok(CaptureBackend {
            capabilities: protocol::VideoCapabilities::new(video::kms::h265_enabled(&card)),
            encoder_label: encoder,
        })
    }
    #[cfg(not(target_os = "linux"))]
    {
        let codecs = video::ram::probe()?;
        Ok(CaptureBackend {
            encoder_label: codecs.h264.clone(),
            capabilities: protocol::VideoCapabilities::new(codecs.h265.is_some()),
            codecs,
        })
    }
}

/// The KVM answers a video registration with video.config.
fn wait_for_video_config(
    worker: &Worker,
    link: &mut ControlLink,
) -> Result<Option<VideoConfig>, Box<dyn Error>> {
    let deadline = Instant::now() + REGISTRATION_TIMEOUT;
    link.serve_until(worker, |record| match record {
        Some(ControlRecord::Config(config)) => Ok(Some(config)),
        _ if Instant::now() >= deadline => {
            Err("the KVM did not answer the video registration".into())
        }
        _ => Ok(None),
    })
}

/// Runs the streaming thread at high priority, so capture keeps its pace
/// while the desktop is busy.
#[cfg(windows)]
fn raise_thread_priority() -> Result<(), Box<dyn Error>> {
    use winapi::um::{
        processthreadsapi::{GetCurrentThread, SetThreadPriority},
        winbase::THREAD_PRIORITY_HIGHEST,
    };
    if unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST as i32) } == 0 {
        let error = std::io::Error::last_os_error();
        return Err(format!("could not raise streaming thread priority: {error}").into());
    }
    Ok(())
}

/// Logs how long the session took to reach a startup phase, so the service
/// log shows where a slow or failed start stopped.
pub fn log_startup(started: &Instant, phase: &str) {
    eprintln!(
        "startup phase={phase} elapsed_ms={}",
        started.elapsed().as_millis()
    );
}
