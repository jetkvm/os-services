//! Windows capture: the DXGI and GDI capturers (`dxgi`), the pointer
//! overlay, and the frames an idle desktop still needs.
//!
//! A capture starts on GDI, which delivers a full first frame even when the
//! desktop is static, and moves to DXGI once that frame arrives. When Windows
//! invalidates the DXGI duplication, for example for the lock screen, a UAC
//! prompt or Ctrl+Alt+Del, or when DXGI does not open, capture goes on with
//! GDI and tries DXGI again once a second.

use super::{
    cursor::CursorCompositor,
    dxgi::{Capturer, Grab},
    FrameClock, FrameJob,
};
use crate::session::worker::Worker;
use crate::video::{display::Display, nv12::Nv12Layout, Captured, Configured, FRAME_INTERVAL};
use std::{
    error::Error,
    time::{Duration, Instant},
};

/// How long capture stays on GDI before it tries DXGI again. On the secure
/// desktop the attempt fails at once.
const DXGI_RETRY: Duration = Duration::from_secs(1);

/// A desktop without a fresh capture for this long is idle. The frame it
/// sends again for a keyframe request is marked as the answer to that
/// request, and the worker keys it.
const IDLE_DESKTOP: Duration = Duration::from_secs(1);

/// A frame an idle desktop sends.
struct IdleFrame {
    nv12: Vec<u8>,
    /// The frame is the re-send that answers a keyframe request.
    resend: bool,
}

pub struct Source {
    /// None between one display and the next, and during a pass. DXGI
    /// allows one duplication of an output per process, so the old capturer
    /// goes before the new one opens.
    capturer: Option<Capturer>,
    width: usize,
    height: usize,
    origin: (i32, i32),
    /// The NV12 layout the encoder reads.
    encoder_layout: Nv12Layout,
    clock: FrameClock,
    /// The last fresh frame: the pointer overlay draws on it, and a
    /// keyframe request on an idle desktop re-sends it.
    last_frame: Vec<u8>,
    cursor: CursorCompositor,
    /// On GDI until its first frame arrives, then to DXGI.
    probing: bool,
    /// On GDI because DXGI was invalidated or did not open: when to try
    /// DXGI again.
    dxgi_retry_at: Option<Instant>,
    /// Windows drew the pointer into the current frame itself, as it does
    /// when it renders the pointer in software.
    pointer_in_frame: bool,
    /// A keyframe request waits for a frame to key.
    keyframe_pending: bool,
    last_fresh_capture: Instant,
    fresh_this_pass: bool,
}

impl Source {
    pub fn open(display: Display, configured: &Configured) -> Result<Self, Box<dyn Error>> {
        let mut source = Self {
            capturer: None,
            width: 0,
            height: 0,
            origin: (0, 0),
            encoder_layout: configured.layout,
            clock: FrameClock::new(),
            last_frame: Vec::new(),
            cursor: CursorCompositor::new()?,
            probing: true,
            dxgi_retry_at: None,
            pointer_in_frame: false,
            keyframe_pending: false,
            last_fresh_capture: Instant::now(),
            fresh_this_pass: false,
        };
        source.reopen(display)?;
        Ok(source)
    }

    /// Captures the display afresh, starting on GDI.
    pub fn reopen(&mut self, display: Display) -> Result<(), Box<dyn Error>> {
        self.close();
        self.bind(&display);
        self.capturer = Some(Capturer::gdi(&display)?);
        self.probing = true;
        self.restart();
        Ok(())
    }

    pub fn close(&mut self) {
        self.capturer = None;
    }

    /// The encoder changed: convert into its layout from the next frame on.
    pub fn set_configured(&mut self, configured: &Configured) {
        self.encoder_layout = configured.layout;
        self.restart();
    }

    /// True once the GDI probe has delivered its frame, and when capture on
    /// GDI is due to try DXGI again.
    pub fn needs_new_capturer(&self) -> bool {
        (self.probing && self.fresh_this_pass)
            || self.dxgi_retry_at.is_some_and(|at| Instant::now() >= at)
    }

    /// Replaces the GDI capturer with a DXGI capturer for the display as it
    /// is now, or with a new GDI capturer when DXGI does not open.
    pub fn renew(&mut self, display: Display) -> Result<(), Box<dyn Error>> {
        self.close();
        self.bind(&display);
        let capturer = if self.dxgi_retry_at.is_some() {
            match Capturer::try_dxgi(&display) {
                Some(dxgi) => {
                    eprintln!("DXGI capture is back");
                    dxgi
                }
                None => Capturer::gdi(&display)?,
            }
        } else {
            let capturer = Capturer::dxgi(display)?;
            if capturer.is_dxgi() {
                eprintln!("GDI readiness frame succeeded; new DXGI capturer created");
            }
            capturer
        };
        self.dxgi_retry_at = (!capturer.is_dxgi()).then(|| Instant::now() + DXGI_RETRY);
        self.capturer = Some(capturer);
        self.probing = false;
        Ok(())
    }

    pub fn keyframe_requested(&mut self) {
        self.cursor.invalidate();
        self.keyframe_pending = true;
    }

    /// A new encoder stream needs a current frame even when DXGI reports no
    /// damage, so capture takes one full GDI frame and moves to a new DXGI
    /// capturer from there. A pending DXGI retry waits for that frame too:
    /// the move to DXGI after it is the retry.
    pub fn stream_reset(&mut self) -> Result<(), Box<dyn Error>> {
        self.dxgi_retry_at = None;
        self.restart();
        self.cursor.reset();
        self.capturer
            .as_mut()
            .expect("capture is open")
            .switch_to_gdi()
            .map_err(|err| format!("GDI capture would not start for the new stream: {err}"))?;
        self.probing = true;
        self.fresh_this_pass = false;
        Ok(())
    }

    /// One capture pass; the capturer blocks for up to one frame interval.
    pub fn next(&mut self, worker: &Worker) -> Result<Captured, Box<dyn Error>> {
        // Out of `self` for the pass, so the capturer can write into the
        // cached frame.
        let mut capturer = self.capturer.take().expect("capture is open");
        let captured = self.pass(&mut capturer, worker);
        self.capturer = Some(capturer);
        captured
    }

    fn pass(
        &mut self,
        capturer: &mut Capturer,
        worker: &Worker,
    ) -> Result<Captured, Box<dyn Error>> {
        self.fresh_this_pass = false;
        let started = Instant::now();
        let layout = self.layout();
        let grabbed = capturer
            .frame(FRAME_INTERVAL, &layout, &mut self.last_frame)
            .map_err(|err| format!("capture failed: {err}"))?;
        self.pointer_in_frame = capturer.pointer_in_frame();
        let (capture_us, capture_wait) = self.clock.pass(started);
        let job = |nv12, capture_wait, convert, fresh, keyframe_resend| FrameJob {
            nv12,
            width: layout.width,
            height: layout.height,
            capture_us,
            capture_wait,
            convert,
            fresh,
            keyframe_resend,
        };
        match grabbed {
            Grab::Frame { convert } => {
                let nv12 = self.fresh_frame(worker)?;
                worker.submit(job(
                    nv12,
                    capture_wait.saturating_sub(convert),
                    convert,
                    true,
                    false,
                ));
                Ok(Captured::Frame)
            }
            Grab::Resized => Ok(Captured::DisplayChanged),
            Grab::Unchanged => {
                self.clock.check_first_frame(!self.last_frame.is_empty())?;
                match self.idle_frame(worker)? {
                    Some(IdleFrame { nv12, resend }) => {
                        worker.submit(job(nv12, capture_wait, Duration::ZERO, false, resend))
                    }
                    None => worker.note_idle(),
                }
                Ok(Captured::Idle)
            }
            Grab::Invalidated => {
                capturer.switch_to_gdi().map_err(|err| {
                    format!("DXGI was invalidated and GDI capture would not start: {err}")
                })?;
                eprintln!("DXGI capture invalidated; capturing through GDI and trying DXGI again every second");
                self.dxgi_retry_at = Some(Instant::now() + DXGI_RETRY);
                Ok(Captured::Recovered)
            }
        }
    }

    fn bind(&mut self, display: &Display) {
        self.width = display.width();
        self.height = display.height();
        self.origin = display.origin();
        self.cursor.reset();
    }

    /// A new frame epoch: the cached frame is stale and a first frame is
    /// awaited again.
    fn restart(&mut self) {
        self.last_frame.clear();
        self.clock.restart();
    }

    fn layout(&self) -> Nv12Layout {
        Nv12Layout {
            width: self.width,
            height: self.height,
            ..self.encoder_layout
        }
    }

    /// The fresh frame that the capturer wrote into the cached NV12 frame,
    /// with the pointer drawn, in a recycled buffer.
    fn fresh_frame(&mut self, worker: &Worker) -> Result<Vec<u8>, Box<dyn Error>> {
        let mut nv12 = worker.take_buffer();
        nv12.clear();
        if self.pointer_in_frame {
            self.cursor.invalidate();
            nv12.extend_from_slice(&self.last_frame);
        } else {
            let (layout, origin) = (self.layout(), self.origin);
            self.cursor
                .draw(true, layout, origin, &mut self.last_frame, |frame| {
                    nv12.extend_from_slice(frame)
                })?
                .ok_or("the pointer overlay did not accept a fresh frame")?;
        }
        self.fresh_this_pass = true;
        self.last_fresh_capture = Instant::now();
        self.keyframe_pending = false;
        Ok(nv12)
    }

    /// The frame an idle desktop sends, if any: the cached frame with the
    /// pointer redrawn after it moved, or the cached frame as it is for a
    /// pending keyframe request.
    fn idle_frame(&mut self, worker: &Worker) -> Result<Option<IdleFrame>, Box<dyn Error>> {
        if self.last_frame.is_empty() || (self.pointer_in_frame && !self.keyframe_pending) {
            return Ok(None);
        }
        let mut nv12 = worker.take_buffer();
        nv12.clear();
        let (layout, origin) = (self.layout(), self.origin);
        let redrawn = !self.pointer_in_frame
            && self
                .cursor
                .draw(false, layout, origin, &mut self.last_frame, |frame| {
                    nv12.extend_from_slice(frame)
                })?
                .is_some();
        if !redrawn && !self.keyframe_pending {
            return Ok(None);
        }
        if !redrawn {
            nv12.extend_from_slice(&self.last_frame);
        }
        let resend = self.keyframe_pending && self.last_fresh_capture.elapsed() >= IDLE_DESKTOP;
        self.keyframe_pending = false;
        Ok(Some(IdleFrame { nv12, resend }))
    }
}
