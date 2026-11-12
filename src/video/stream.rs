//! The video service while it is registered: places the KVM display, keeps
//! the encoder configured for it, and follows the KVM's video.config until
//! the session ends or video fails.

use super::{
    display::{self, describe, Display, DisplaySelector},
    Captured, Configured, EncoderRequest, Source, FPS,
};
use crate::protocol::{ControlRecord, VideoConfig};
use crate::session::{control::ControlLink, log_startup, worker::Worker};
use std::{
    error::Error,
    thread,
    time::{Duration, Instant},
};

/// How long the KVM display has to appear after a re-plug.
const DISPLAY_TIMEOUT: Duration = Duration::from_secs(10);
const DISPLAY_POLL: Duration = Duration::from_millis(250);
/// How long after an apply of the KVM the sender keeps the KVM display at
/// the applied size. A desktop takes a presented display in at its own
/// pace: it can list the display of before, or the new display at the mode
/// of the one before, for a moment.
const MODE_SET_WINDOW: Duration = Duration::from_secs(10);
/// How often the display's mode is compared with the capture.
const MODE_CHECK_INTERVAL: Duration = Duration::from_secs(1);
/// How long capture waits for the desktop to give a presented KVM display
/// the size it adopts by itself, and how often it looks.
const MODE_ADOPT_TIMEOUT: Duration = Duration::from_secs(2);
const MODE_ADOPT_POLL: Duration = Duration::from_millis(50);

/// Streams the registered video service until the session ends (Ok) or
/// video fails (Err). The caller asks the worker whether the failure was the
/// wire's.
pub fn stream_video(
    startup: &Instant,
    device: &str,
    encoder_label: &str,
    selector: &mut DisplaySelector,
    config: VideoConfig,
    worker: &Worker,
    link: &mut ControlLink,
) -> Result<(), Box<dyn Error>> {
    let Some(config) = wait_for_video_enabled(config, worker, link)? else {
        return Ok(());
    };
    let sets_mode = link.sets_display_mode();
    let mut mode_due = mode_window(sets_mode);
    let Some(display) = place_display(selector, config, &mut mode_due, true, worker, link)? else {
        return Ok(());
    };
    let (width, height) = (display.width(), display.height());
    log_startup(startup, "display_ready");
    let configured = worker.configure(request(selector, width, height, config), link)?;
    log_startup(startup, "encoder_ready");
    let source = Source::open(display, &configured)?;
    log_startup(startup, "capturer_ready");
    eprintln!(
        "usb={device} display={width}x{height} encoder={encoder_label} bitrate={}kbit/s native_fps={FPS}",
        configured.bitrate
    );
    VideoStream {
        worker,
        link,
        selector,
        config,
        width,
        height,
        source,
        next_mode_check: Instant::now() + MODE_CHECK_INTERVAL,
        reconfigure: false,
        sets_mode,
        mode_due,
        presented: false,
    }
    .run()
}

fn request(
    selector: &DisplaySelector,
    width: usize,
    height: usize,
    config: VideoConfig,
) -> EncoderRequest {
    EncoderRequest {
        display: selector.selected_name().to_owned(),
        width,
        height,
        config,
    }
}

struct VideoStream<'a, 'c> {
    worker: &'a Worker,
    link: &'a mut ControlLink<'c>,
    selector: &'a mut DisplaySelector,
    config: VideoConfig,
    width: usize,
    height: usize,
    source: Source,
    next_mode_check: Instant,
    /// The display is placed again before the next frame.
    reconfigure: bool,
    /// The sender told the KVM that it sets the mode of the KVM display.
    sets_mode: bool,
    /// Until when the sender sets the display's mode: the KVM applied its
    /// settings less than MODE_SET_WINDOW ago. None otherwise, so a mode
    /// the user picks stays.
    mode_due: Option<Instant>,
    /// The KVM presents the display again (video enabled again, or a new
    /// resolution or layout): the next placement waits for the mode that
    /// the desktop adopts for it.
    presented: bool,
}

impl VideoStream<'_, '_> {
    fn run(mut self) -> Result<(), Box<dyn Error>> {
        loop {
            if self.link.shutdown_requested() {
                return Ok(());
            }
            if let Some(failure) = self.worker.failure() {
                return Err(failure.into());
            }
            if let Some(fault) = self.worker.take_fault() {
                return Err(fault.into());
            }
            self.reconfigure |= self.selector.update_requested(self.link.capture_display());
            if (self.reconfigure || Instant::now() >= self.next_mode_check)
                && !self.follow_display()?
            {
                return Ok(());
            }
            match self.source.next(self.worker)? {
                Captured::DisplayChanged => {
                    self.reconfigure = true;
                    continue;
                }
                Captured::Recovered => continue,
                Captured::Frame | Captured::Idle => {}
            }
            let record = self.link.next(self.worker, Duration::ZERO)?;
            if !self.handle(record)? {
                return Ok(());
            }
            if self.source.needs_new_capturer() && !self.renew_capturer()? {
                return Ok(());
            }
        }
    }

    /// Once a second, and at once when asked: the display changes size when
    /// the KVM swaps its EDID or the user picks a mode, and capture follows.
    /// The layout is applied again to a display that comes back after an
    /// EDID swap. False when the session is stopping.
    fn follow_display(&mut self) -> Result<bool, Box<dyn Error>> {
        self.next_mode_check = Instant::now() + MODE_CHECK_INTERVAL;
        // A mode set here changes the display's size, which the check below
        // sees.
        set_due_mode(&mut self.mode_due, self.config);
        let changed =
            self.reconfigure || self.selector.selected_mode()? != Some((self.width, self.height));
        self.reconfigure = false;
        if !changed {
            return Ok(true);
        }
        let Some(display) = place_display(
            self.selector,
            self.config,
            &mut self.mode_due,
            std::mem::take(&mut self.presented),
            self.worker,
            self.link,
        )?
        else {
            return Ok(false);
        };
        (self.width, self.height) = (display.width(), display.height());
        self.source.close();
        let bitrate = self.configure_encoder()?;
        self.source.reopen(display)?;
        eprintln!(
            "capture display reconfigured: {}x{} bitrate={bitrate}kbit/s",
            self.width, self.height
        );
        Ok(true)
    }

    /// Creates the encoder for the current size; the capture converts into
    /// the layout it reports. Returns the bitrate.
    fn configure_encoder(&mut self) -> Result<u32, Box<dyn Error>> {
        let request = request(self.selector, self.width, self.height, self.config);
        let configured: Configured = self.worker.configure(request, self.link)?;
        self.source.set_configured(&configured);
        Ok(configured.bitrate)
    }

    /// Acts on a record for the video service. False when the session is
    /// stopping.
    fn handle(&mut self, record: Option<ControlRecord>) -> Result<bool, Box<dyn Error>> {
        match record {
            Some(ControlRecord::KeyframeRequest) => {
                self.worker.request_keyframe();
                self.source.keyframe_requested();
            }
            Some(ControlRecord::Config(config)) if !config.video_enabled => {
                let Some(config) = wait_for_video_enabled(config, self.worker, self.link)? else {
                    return Ok(false);
                };
                self.config = config;
                // The KVM re-plugs the display when video is enabled again;
                // the next pass places it once its mode settles.
                self.reconfigure = true;
                self.mode_due = mode_window(self.sets_mode);
                self.presented = true;
            }
            Some(ControlRecord::Config(config)) => self.apply_config(config)?,
            _ => {}
        }
        Ok(true)
    }

    /// A new resolution or layout arrives around the time the KVM re-plugs
    /// the display with that timing preferred; the next pass places the
    /// display and sets its mode once it is the one with that timing. A
    /// codec change keeps the display and replaces the encoder. A quality
    /// target changes only the encoder.
    fn apply_config(&mut self, config: VideoConfig) -> Result<(), Box<dyn Error>> {
        let current = std::mem::replace(&mut self.config, config);
        if (config.width, config.height, config.layout)
            != (current.width, current.height, current.layout)
        {
            self.reconfigure = true;
            self.mode_due = mode_window(self.sets_mode);
            self.presented = true;
        } else if config.codec != current.codec {
            self.configure_encoder()?;
            self.source.stream_reset()?;
        } else if (config.target_kbps, config.maximum_fps)
            != (current.target_kbps, current.maximum_fps)
            && self.worker.apply_quality(config, self.link)?
        {
            self.source.stream_reset()?;
        }
        Ok(())
    }

    /// Binds a new capturer to the display as it is now, when the source
    /// asks for one. False when the session is stopping.
    fn renew_capturer(&mut self) -> Result<bool, Box<dyn Error>> {
        self.source.close();
        let Some(display) = wait_for_display(self.selector, self.worker, self.link)? else {
            return Ok(false);
        };
        if (display.width(), display.height()) != (self.width, self.height) {
            (self.width, self.height) = (display.width(), display.height());
            self.configure_encoder()?;
        }
        self.source.renew(display)?;
        Ok(true)
    }
}

/// Holds while the KVM has video disabled. The worker drops the encoder,
/// and with it any capture, before it acknowledges: the KVM removes the
/// display right after, and no capture may hold the scanout while the
/// desktop loses its monitor. Pings and the other services go on meanwhile.
fn wait_for_video_enabled(
    mut config: VideoConfig,
    worker: &Worker,
    link: &mut ControlLink,
) -> Result<Option<VideoConfig>, Box<dyn Error>> {
    if !config.video_enabled {
        eprintln!("the KVM has video off; waiting for it to turn video on");
    }
    while !config.video_enabled {
        worker.stop_capture(config, false, link)?;
        let next = link.serve_until(worker, |record| {
            Ok(match record {
                Some(ControlRecord::Config(next)) => Some(next),
                _ => None,
            })
        })?;
        let Some(next) = next else {
            return Ok(None);
        };
        config = next;
    }
    Ok(Some(config))
}

/// Waits for the KVM display, sets its mode when that is due, places it
/// relative to any other display, and waits for it again: both can change
/// the size the desktop gives the display, so capture binds only to the
/// display as it is afterwards. A display that the KVM has just presented
/// (`presented`) is also given the time to take the mode that the desktop
/// adopts for it. A desktop that refuses the mode or the placement is
/// streamed as it is.
fn place_display(
    selector: &mut DisplaySelector,
    config: VideoConfig,
    mode_due: &mut Option<Instant>,
    presented: bool,
    worker: &Worker,
    link: &mut ControlLink,
) -> Result<Option<Display>, Box<dyn Error>> {
    let Some(name) = wait_for_display(selector, worker, link)?.map(|display| display.name()) else {
        return Ok(None);
    };
    set_due_mode(mode_due, config);
    if let Err(error) = display::apply_layout(&name, config) {
        eprintln!("requested display layout not applied: {error}");
    }
    let Some(display) = wait_for_display(selector, worker, link)? else {
        return Ok(None);
    };
    if !presented {
        return Ok(Some(display));
    }
    wait_for_adopted_mode(display, config, selector, worker, link)
}

/// Waits, up to MODE_ADOPT_TIMEOUT, until the display has the size that the
/// desktop gives it by itself (display::adopted_size). Capture that opened
/// at the size of before would be rebuilt when the mode arrives, and the
/// KVM would get a new stream for each. A display that goes offline in the
/// meantime is waited for; one still offline at the end is selected again
/// as wait_for_display does.
fn wait_for_adopted_mode(
    display: Display,
    config: VideoConfig,
    selector: &mut DisplaySelector,
    worker: &Worker,
    link: &mut ControlLink,
) -> Result<Option<Display>, Box<dyn Error>> {
    let name = display.name();
    let Some(size) = display::adopted_size(&name, config) else {
        return Ok(Some(display));
    };
    let started = Instant::now();
    let mut current = Some(display);
    loop {
        if let Some(display) = current.take_if(|d| (d.width(), d.height()) == size) {
            eprintln!(
                "capture display {name} has {}x{} after {} ms",
                size.0,
                size.1,
                started.elapsed().as_millis()
            );
            return Ok(Some(display));
        }
        if started.elapsed() >= MODE_ADOPT_TIMEOUT {
            let Some(display) = current else {
                return wait_for_display(selector, worker, link);
            };
            eprintln!(
                "capture display {name} kept {}x{}; the desktop did not adopt {}x{}",
                display.width(),
                display.height(),
                size.0,
                size.1
            );
            return Ok(Some(display));
        }
        if link.shutdown_requested() {
            return Ok(None);
        }
        link.serve(worker)?;
        thread::sleep(MODE_ADOPT_POLL);
        current = display::find_online(&name)?;
    }
}

/// The time until which the mode is set after an apply of the KVM, for a
/// sender that told the KVM that it sets the mode.
fn mode_window(sets_mode: bool) -> Option<Instant> {
    sets_mode.then(|| Instant::now() + MODE_SET_WINDOW)
}

/// Sets the mode of the KVM display to the size in `config` until the time
/// in `mode_due`: the stream asks at each placement and once a second. A
/// desktop can refuse a mode while it takes the display in, so a refusal is
/// logged and the next pass asks again; after the time the display keeps
/// the mode it has.
fn set_due_mode(mode_due: &mut Option<Instant>, config: VideoConfig) {
    if mode_due.is_none_or(|deadline| Instant::now() >= deadline) {
        *mode_due = None;
    } else if let Err(error) = display::apply_mode(config) {
        eprintln!("requested display mode not set: {error}");
    }
}

/// Serves the session, keeping records for the streaming loop, until the
/// KVM display can be selected; fails after DISPLAY_TIMEOUT.
fn wait_for_display(
    selector: &mut DisplaySelector,
    worker: &Worker,
    link: &mut ControlLink,
) -> Result<Option<Display>, Box<dyn Error>> {
    let deadline = Instant::now() + DISPLAY_TIMEOUT;
    let mut seen = String::new();
    while Instant::now() < deadline {
        if link.shutdown_requested() {
            return Ok(None);
        }
        if let Some(failure) = worker.failure() {
            return Err(failure.into());
        }
        selector.update_requested(link.capture_display());
        let displays = Display::all()?;
        seen = describe(&displays);
        if let Some(display) = selector.select(displays)? {
            selector.log_selection(seen);
            return Ok(Some(display));
        }
        link.serve(worker)?;
        thread::sleep(DISPLAY_POLL);
    }
    Err(format!("KVM display not found; detected: {seen}").into())
}
