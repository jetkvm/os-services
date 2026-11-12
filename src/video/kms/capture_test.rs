//! A test tool for the KMS pipeline on a machine without a KVM:
//!
//! ```text
//! ipkvm-os-services --capture-test OUT SECONDS
//!     [--capture-test-retarget-after SECONDS KBPS]
//!     [--capture-test-display NAME] [--capture-test-h265]
//! ```
//!
//! It runs what a session runs, from the probe through configure, steps and
//! reopens, and writes the encoded stream to OUT as raw Annex-B instead of
//! to the KVM. The retarget gives the running encoder a new bitrate, as the
//! KVM does when its network changes. Each video.status the pipeline writes
//! goes to stderr, so a reopen and the size it opened at show there. The
//! NVENC pipeline also prints its per-stage frame times every 5 seconds.

use super::{probe, Backend, KmsPipeline};
use crate::protocol::{
    codec::Message, DisplayLayout, Transmit, VideoCodec, VideoConfig, VIDEO_SERVICE, VIDEO_STATUS,
};
use crate::session::worker::{FailureSlot, Wire};
use crate::video::{display, EncoderRequest, Pipeline};
use std::{
    error::Error,
    fs::File,
    io::{BufWriter, Write},
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

pub const FLAG: &str = "--capture-test";

const USAGE: &str = "usage: ipkvm-os-services --capture-test OUT SECONDS \
                     [--capture-test-retarget-after SECONDS KBPS] \
                     [--capture-test-display NAME] [--capture-test-h265]";

struct Options {
    out: String,
    run_for: Duration,
    retarget: Option<(Duration, u32)>,
    display: Option<String>,
    codec: VideoCodec,
}

fn seconds(text: &str) -> Result<Duration, String> {
    text.parse::<f64>()
        .ok()
        .and_then(|value| Duration::try_from_secs_f64(value).ok())
        .ok_or_else(|| format!("{text} is not a number of seconds"))
}

fn parse(args: &[&str]) -> Result<Options, String> {
    let [FLAG, out, run_for, rest @ ..] = args else {
        return Err(USAGE.into());
    };
    let mut options = Options {
        out: (*out).to_owned(),
        run_for: seconds(run_for)?,
        retarget: None,
        display: None,
        codec: VideoCodec::H264,
    };
    let mut rest = rest;
    while !rest.is_empty() {
        rest = match rest {
            ["--capture-test-retarget-after", after, kbps, tail @ ..] => {
                let kbps = kbps
                    .parse()
                    .map_err(|_| format!("{kbps} is not a bitrate"))?;
                options.retarget = Some((seconds(after)?, kbps));
                tail
            }
            ["--capture-test-display", name, tail @ ..] => {
                options.display = Some((*name).to_owned());
                tail
            }
            ["--capture-test-h265", tail @ ..] => {
                options.codec = VideoCodec::H265;
                tail
            }
            _ => return Err(USAGE.into()),
        };
    }
    Ok(options)
}

/// What the stream wrote, shared with the sink the wire owns.
#[derive(Default)]
struct Counts {
    units: AtomicUsize,
    keyframes: AtomicUsize,
    bytes: AtomicU64,
}

impl Counts {
    fn report(&self, elapsed: Duration) {
        let seconds = elapsed.as_secs_f64();
        let units = self.units.load(Ordering::Relaxed);
        let bytes = self.bytes.load(Ordering::Relaxed);
        eprintln!(
            "capture-test: {units} access units ({} keyframes), {bytes} bytes in {seconds:.1}s: {:.1} fps, {:.0} kbps",
            self.keyframes.load(Ordering::Relaxed),
            units as f64 / seconds,
            bytes as f64 * 8.0 / 1000.0 / seconds,
        );
    }
}

/// The KVM's side of the wire: access units go to the file, and each
/// video.status to stderr.
struct FileSink {
    out: BufWriter<File>,
    counts: Arc<Counts>,
    started: Instant,
    cancelled: Arc<AtomicBool>,
}

impl Transmit for FileSink {
    fn cancellation(&self) -> &Arc<AtomicBool> {
        &self.cancelled
    }

    fn send(&mut self, message: &Message) -> Result<(), Box<dyn Error>> {
        if (message.service, message.kind) != (VIDEO_SERVICE, VIDEO_STATUS) {
            return Ok(());
        }
        // The layout of codec::video_status.
        let field = |index: usize| {
            let at = 4 + index * 4;
            message
                .payload
                .get(at..at + 4)
                .map_or(0, |bytes| u32::from_le_bytes(bytes.try_into().unwrap()))
        };
        eprintln!(
            "capture-test: {:.2}s video.status {}x{} stream_reset={} default_kbps={} target_kbps={} maximum_fps={}",
            self.started.elapsed().as_secs_f64(),
            field(0),
            field(1),
            message.payload.get(3) == Some(&1),
            field(3),
            field(4),
            field(5),
        );
        Ok(())
    }

    fn send_frame(
        &mut self,
        unit: &[u8],
        _capture_us: u64,
        keyframe: bool,
        _parameter_sets: bool,
    ) -> Result<(), Box<dyn Error>> {
        self.out.write_all(unit)?;
        self.counts.units.fetch_add(1, Ordering::Relaxed);
        self.counts
            .bytes
            .fetch_add(unit.len() as u64, Ordering::Relaxed);
        if keyframe {
            self.counts.keyframes.fetch_add(1, Ordering::Relaxed);
            eprintln!(
                "capture-test: {:.2}s keyframe, {} bytes",
                self.started.elapsed().as_secs_f64(),
                unit.len()
            );
        }
        Ok(())
    }
}

impl Drop for FileSink {
    fn drop(&mut self) {
        if let Err(error) = self.out.flush() {
            eprintln!("capture-test: could not write the stream: {error}");
        }
    }
}

/// The display to capture: the named one, or the one a session picks.
fn capture_display(name: Option<&str>) -> Result<display::Display, Box<dyn Error>> {
    if let Some(name) = name {
        return Ok(display::find_online(name)?.ok_or(format!("display {name} is not online"))?);
    }
    let displays = display::Display::all()?;
    let described = display::describe(&displays);
    display::DisplaySelector::new(None)
        .select(displays)?
        .ok_or_else(|| {
            format!("no display to capture ({described}); name one with --capture-test-display")
                .into()
        })
}

fn quality_config(codec: VideoCodec, (width, height): (usize, usize), kbps: u32) -> VideoConfig {
    VideoConfig {
        video_enabled: true,
        layout: DisplayLayout::Mirror,
        codec,
        width: width as u32,
        height: height as u32,
        target_kbps: kbps,
        maximum_fps: 0,
    }
}

fn capture(options: Options) -> Result<(), Box<dyn Error>> {
    let probed = probe().map_err(|why| format!("KMS capture probe failed: {why}"))?;
    let backend = Backend::for_card(&probed.card);
    eprintln!(
        "capture-test: probe found {} ({backend:?}, H.264 with {})",
        probed.card, probed.encoder
    );
    let display = capture_display(options.display.as_deref())?;
    let size = (display.width(), display.height());
    eprintln!(
        "capture-test: capturing {} at {}x{} for {:?} into {}",
        display.name(),
        size.0,
        size.1,
        options.run_for,
        options.out
    );

    let counts = Arc::new(Counts::default());
    let started = Instant::now();
    let sink = FileSink {
        out: BufWriter::new(File::create(&options.out)?),
        counts: counts.clone(),
        started,
        cancelled: Arc::default(),
    };
    let (mut wire, _replier) = Wire::with_replier(sink);
    let fault = FailureSlot::default();
    let mut pipeline = KmsPipeline::new(fault.clone());
    let configured = pipeline.configure(
        EncoderRequest {
            display: display.name(),
            width: size.0,
            height: size.1,
            config: quality_config(options.codec, size, 0),
        },
        &mut wire,
    )?;
    eprintln!("capture-test: configured at {} kbps", configured.bitrate);

    let streamed = stream(&mut pipeline, &mut wire, &fault, &options, size, started);
    pipeline.stop();
    counts.report(started.elapsed());
    streamed
}

/// Steps the pipeline until the run time is up, retargeting it on the way.
fn stream(
    pipeline: &mut KmsPipeline,
    wire: &mut Wire,
    fault: &FailureSlot,
    options: &Options,
    size: (usize, usize),
    started: Instant,
) -> Result<(), Box<dyn Error>> {
    let mut retarget = options.retarget;
    while started.elapsed() < options.run_for {
        if let Some((after, kbps)) = retarget.filter(|(after, _)| started.elapsed() >= *after) {
            retarget = None;
            eprintln!("capture-test: {after:?} in, retargeting to {kbps} kbps");
            let reset = pipeline.apply_quality(quality_config(options.codec, size, kbps), wire)?;
            eprintln!("capture-test: retarget done, stream_reset={reset}");
        }
        pipeline.step(wire, false);
        if let Some(failure) = fault.get() {
            return Err(format!("the pipeline failed: {failure}").into());
        }
        if wire.failed() {
            return Err("writing the stream failed".into());
        }
    }
    Ok(())
}

/// Runs the capture test for the arguments after the program name; returns
/// the process exit code.
pub fn run(args: &[&str]) -> i32 {
    // The NVENC pipeline prints its per-stage frame times with this set.
    if std::env::var_os("OS_SERVICES_KMS_STAGES").is_none() {
        // SAFETY: no other thread exists yet.
        unsafe { std::env::set_var("OS_SERVICES_KMS_STAGES", "1") };
    }
    let result = parse(args).map_err(Into::into).and_then(capture);
    match result {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("capture-test: {error}");
            1
        }
    }
}
