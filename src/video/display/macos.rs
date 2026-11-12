//! macOS displays, named by their CGDirectDisplayID and sized in pixels.

use core_graphics::display::{CGDirectDisplayID, CGDisplay};
use std::io;

extern "C" {
    fn CGGetOnlineDisplayList(
        max_displays: u32,
        displays: *mut CGDirectDisplayID,
        count: *mut u32,
    ) -> i32;
    fn CGDisplayIsOnline(display: CGDirectDisplayID) -> u32;
}

pub struct Display(CGDirectDisplayID);

impl Display {
    pub fn all() -> io::Result<Vec<Self>> {
        let mut ids = [0; 32];
        let mut count = 0;
        let error =
            unsafe { CGGetOnlineDisplayList(ids.len() as u32, ids.as_mut_ptr(), &mut count) };
        if error != 0 {
            return Err(io::Error::other(format!(
                "CGGetOnlineDisplayList failed: {error}"
            )));
        }
        Ok(ids[..count as usize].iter().map(|&id| Self(id)).collect())
    }

    pub fn id(&self) -> CGDirectDisplayID {
        self.0
    }

    pub fn name(&self) -> String {
        self.0.to_string()
    }

    /// The width of the current mode in pixels, which is twice its width
    /// in points on a Retina mode.
    pub fn width(&self) -> usize {
        let display = CGDisplay::new(self.0);
        display
            .display_mode()
            .map_or(display.pixels_wide(), |mode| mode.pixel_width()) as usize
    }

    pub fn height(&self) -> usize {
        let display = CGDisplay::new(self.0);
        display
            .display_mode()
            .map_or(display.pixels_high(), |mode| mode.pixel_height()) as usize
    }

    pub fn origin(&self) -> (i32, i32) {
        let origin = CGDisplay::new(self.0).bounds().origin;
        (origin.x as i32, origin.y as i32)
    }

    pub fn is_online(&self) -> bool {
        unsafe { CGDisplayIsOnline(self.0) != 0 }
    }

    pub fn is_primary(&self) -> bool {
        CGDisplay::new(self.0).is_main()
    }
}
