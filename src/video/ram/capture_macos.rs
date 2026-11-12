//! macOS capture: a ScreenCaptureKit stream that delivers NV12 with the
//! pointer drawn in, copied straight into a buffer the worker encodes.

use super::{FrameClock, FrameJob};
use crate::macos::screen_capture::{Frame, Next, ScreenCapture};
use crate::session::worker::Worker;
use crate::video::{display::Display, nv12::Nv12Layout, Captured, Configured, FPS, FRAME_INTERVAL};
use std::{
    error::Error,
    time::{Duration, Instant},
};

pub struct Source {
    /// None between one display and the next.
    capture: Option<ScreenCapture>,
    width: usize,
    height: usize,
    layout: Nv12Layout,
    clock: FrameClock,
    /// The newest frame. ScreenCaptureKit delivers nothing while the screen
    /// does not change, so an idle desktop sends this frame again.
    last: Option<Frame>,
    /// The encoder waits for a frame: the first one after a restart, or the
    /// answer to a keyframe request. An idle desktop sends `last`.
    owed: bool,
    /// A frame went to the encoder since the last restart.
    sent: bool,
}

impl Source {
    pub fn open(display: Display, configured: &Configured) -> Result<Self, Box<dyn Error>> {
        let mut source = Self {
            capture: None,
            width: 0,
            height: 0,
            layout: configured.layout,
            clock: FrameClock::new(),
            last: None,
            owed: false,
            sent: false,
        };
        source.reopen(display)?;
        Ok(source)
    }

    /// Captures the display afresh: one that came back after an EDID swap,
    /// or the same one for a new encoder.
    pub fn reopen(&mut self, display: Display) -> Result<(), Box<dyn Error>> {
        self.close();
        self.width = display.width();
        self.height = display.height();
        self.capture = Some(ScreenCapture::start(
            display.id(),
            self.width,
            self.height,
            FPS,
        )?);
        self.restart();
        Ok(())
    }

    /// Ends the stream; its frames have the old display's size.
    pub fn close(&mut self) {
        self.capture = None;
        self.last = None;
    }

    /// The encoder changed: copy into its layout from the next frame on.
    pub fn set_configured(&mut self, configured: &Configured) {
        self.layout = configured.layout;
        self.restart();
    }

    fn restart(&mut self) {
        self.clock.restart();
        self.sent = false;
        self.owed = true;
    }

    /// One capture pass; the stream waits up to one frame interval for a
    /// frame.
    pub fn next(&mut self, worker: &Worker) -> Result<Captured, Box<dyn Error>> {
        let capture = self.capture.as_mut().expect("capture is open");
        let started = Instant::now();
        let frame = capture.next(FRAME_INTERVAL);
        let (capture_us, capture_wait) = self.clock.pass(started);
        match frame {
            Next::Frame(frame) => {
                self.submit(worker, &frame, true, capture_us, capture_wait)?;
                self.last = Some(frame);
                Ok(Captured::Frame)
            }
            Next::Idle => {
                match self.last.take() {
                    Some(frame) if self.owed => {
                        let submitted =
                            self.submit(worker, &frame, false, capture_us, capture_wait);
                        self.last = Some(frame);
                        submitted?;
                    }
                    last => {
                        self.last = last;
                        self.clock.check_first_frame(self.sent)?;
                        worker.note_idle();
                    }
                }
                Ok(Captured::Idle)
            }
            Next::Stopped(reason) => {
                eprintln!("screen capture stopped: {reason}");
                Ok(Captured::DisplayChanged)
            }
        }
    }

    /// Copies `frame` in the encoder's layout to the worker: a fresh capture,
    /// or the newest frame again for an encoder that waits for one, which
    /// the worker keys.
    fn submit(
        &mut self,
        worker: &Worker,
        frame: &Frame,
        fresh: bool,
        capture_us: u64,
        capture_wait: Duration,
    ) -> Result<(), Box<dyn Error>> {
        let started = Instant::now();
        let mut nv12 = worker.take_buffer();
        frame.copy_into(&mut nv12, &self.layout)?;
        self.sent = true;
        self.owed = false;
        worker.submit(FrameJob {
            nv12,
            width: self.width,
            height: self.height,
            capture_us,
            capture_wait,
            convert: started.elapsed(),
            fresh,
            keyframe_resend: !fresh,
        });
        Ok(())
    }

    pub fn keyframe_requested(&mut self) {
        self.owed = true;
    }

    /// A new encoder stream needs a current frame even on an idle desktop.
    pub fn stream_reset(&mut self) -> Result<(), Box<dyn Error>> {
        self.owed = true;
        Ok(())
    }

    pub fn needs_new_capturer(&self) -> bool {
        false
    }

    pub fn renew(&mut self, display: Display) -> Result<(), Box<dyn Error>> {
        self.reopen(display)
    }
}
