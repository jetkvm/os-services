//! The credit window that paces a transfer the KVM relays: the KVM sends
//! only bytes the sender has offered, and the sender offers more as it
//! writes them.

/// Bytes the sender accepts ahead of consumption per transfer. The KVM
/// relays this window to the browser, so it also bounds what waits in the
/// KVM's queue: large enough to cover the credit round trip, small enough
/// that a stalled USB write cannot hold megabytes in the KVM's small memory.
pub const WINDOW: u32 = 512 << 10;
/// Credit is returned in quarter-window steps: often enough that the sender
/// never idles, rarely enough that credit messages stay a small fraction of
/// the chunks they answer.
pub const STEP: u64 = (WINDOW / 4) as u64;

#[derive(Debug)]
pub struct Credit {
    /// Bytes the KVM may send in total, counted from the first byte, and
    /// bytes written since the last credit message.
    granted: u64,
    consumed: u64,
}

impl Credit {
    /// The window offered when a transfer opens.
    pub fn new() -> Self {
        Self {
            granted: u64::from(WINDOW),
            consumed: 0,
        }
    }

    /// Whether a chunk that ends at `end` stays inside the credit.
    pub fn covers(&self, end: u64) -> bool {
        end <= self.granted
    }

    /// Records written bytes. Returns the credit to send back once a whole
    /// step has been written.
    pub fn consume(&mut self, bytes: usize) -> Option<u32> {
        self.consumed += bytes as u64;
        if self.consumed < STEP {
            return None;
        }
        let returned = self.consumed;
        self.granted += returned;
        self.consumed = 0;
        Some(returned as u32)
    }

    #[cfg(test)]
    pub fn granted(&self) -> u64 {
        self.granted
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credit_returns_whole_steps_of_written_bytes() {
        let mut credit = Credit::new();
        assert!(credit.covers(u64::from(WINDOW)));
        assert!(!credit.covers(u64::from(WINDOW) + 1));
        assert_eq!(credit.consume(STEP as usize / 2), None);
        assert_eq!(credit.consume(STEP as usize / 2), Some(STEP as u32));
        assert!(credit.covers(u64::from(WINDOW) + STEP));
        assert_eq!(credit.consume(1), None);
    }
}
