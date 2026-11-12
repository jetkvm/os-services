//! Steady-state stage timing for the streaming hot path. Every value is a
//! duration measured on this process's monotonic clock; nothing is inferred
//! from another machine's clock. Aggregates print to stderr, which is the
//! service's log. tools/perf reads the stage lines from that log.
use std::time::{Duration, Instant};

const REPORT_INTERVAL: Duration = Duration::from_secs(5);
const WINDOW_CAPACITY: usize = 1024;

pub struct FrameSample {
    pub capture_wait: Duration,
    pub convert: Duration,
    pub encode: Duration,
    pub write: Duration,
    pub bytes: usize,
    pub keyframe: bool,
    pub fresh: bool,
}

pub struct StageWindow {
    window_started: Instant,
    last_write_done: Option<Instant>,
    capture_wait: Vec<f32>,
    convert: Vec<f32>,
    encode: Vec<f32>,
    write: Vec<f32>,
    write_gap: Vec<f32>,
    fresh: u32,
    cursor_only: u32,
    idle: u32,
    capped: u32,
    late: u32,
    keyframes: u32,
    bytes: u64,
}

impl StageWindow {
    pub fn new() -> Self {
        Self {
            window_started: Instant::now(),
            last_write_done: None,
            capture_wait: Vec::with_capacity(WINDOW_CAPACITY),
            convert: Vec::with_capacity(WINDOW_CAPACITY),
            encode: Vec::with_capacity(WINDOW_CAPACITY),
            write: Vec::with_capacity(WINDOW_CAPACITY),
            write_gap: Vec::with_capacity(WINDOW_CAPACITY),
            fresh: 0,
            cursor_only: 0,
            idle: 0,
            capped: 0,
            late: 0,
            keyframes: 0,
            bytes: 0,
        }
    }

    /// Capture loop iterations that produced no frame (static desktop).
    pub fn add_idle(&mut self, count: u32) {
        self.idle += count;
    }

    /// Frames displaced from the encode mailbox by a newer frame before the
    /// worker could take them; each one traded completeness for latency.
    #[cfg(not(target_os = "linux"))]
    pub fn add_late(&mut self, count: u32) {
        self.late += count;
    }

    /// A captured frame dropped by the KVM-requested frame-rate cap before
    /// encoding.
    #[cfg(not(target_os = "linux"))]
    pub fn record_capped(&mut self, capture_wait: Duration) {
        self.capped += 1;
        if self.capture_wait.len() < WINDOW_CAPACITY {
            self.capture_wait.push(capture_wait.as_secs_f32() * 1_000.0);
        }
    }

    pub fn record_frame(&mut self, sample: FrameSample) {
        if sample.fresh {
            self.fresh += 1;
        } else {
            self.cursor_only += 1;
        }
        if sample.keyframe {
            self.keyframes += 1;
        }
        self.bytes += sample.bytes as u64;
        let now = Instant::now();
        if let Some(previous) = self.last_write_done.replace(now) {
            let gap = (now - previous).as_secs_f32() * 1_000.0;
            if self.write_gap.len() < WINDOW_CAPACITY {
                self.write_gap.push(gap);
            }
        }
        for (value, values) in [
            (sample.capture_wait, &mut self.capture_wait),
            (sample.convert, &mut self.convert),
            (sample.encode, &mut self.encode),
            (sample.write, &mut self.write),
        ] {
            if values.len() < WINDOW_CAPACITY {
                values.push(value.as_secs_f32() * 1_000.0);
            }
        }
    }

    pub fn maybe_report(&mut self) {
        let elapsed = self.window_started.elapsed();
        if elapsed < REPORT_INTERVAL {
            return;
        }
        let seconds = elapsed.as_secs_f32();
        let encoded = self.fresh + self.cursor_only;
        eprintln!(
            "stage window={:.1}s fresh={} cursor={} idle={} capped={} late={} kf={} fps={:.1} kbps={:.0} \
             cap{} conv{} enc{} usb{} gap{}",
            seconds,
            self.fresh,
            self.cursor_only,
            self.idle,
            self.capped,
            self.late,
            self.keyframes,
            encoded as f32 / seconds,
            self.bytes as f32 * 8.0 / seconds / 1_000.0,
            summarize(&mut self.capture_wait),
            summarize(&mut self.convert),
            summarize(&mut self.encode),
            summarize(&mut self.write),
            summarize(&mut self.write_gap),
        );
        for values in [
            &mut self.capture_wait,
            &mut self.convert,
            &mut self.encode,
            &mut self.write,
            &mut self.write_gap,
        ] {
            values.clear();
        }
        (
            self.fresh,
            self.cursor_only,
            self.idle,
            self.capped,
            self.late,
            self.keyframes,
            self.bytes,
        ) = (0, 0, 0, 0, 0, 0, 0);
        self.window_started = Instant::now();
    }
}

fn summarize(values: &mut [f32]) -> String {
    if values.is_empty() {
        return "[-]".to_owned();
    }
    values.sort_unstable_by(|a, b| a.total_cmp(b));
    let rank = |quantile: f32| {
        let index = ((values.len() as f32) * quantile).ceil() as usize;
        values[index.clamp(1, values.len()) - 1]
    };
    format!(
        "[p50={:.2} p95={:.2} max={:.2}]",
        rank(0.5),
        rank(0.95),
        values[values.len() - 1],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summarize_uses_nearest_rank() {
        let mut values: Vec<f32> = (1..=100).map(|value| value as f32).collect();
        assert_eq!(summarize(&mut values), "[p50=50.00 p95=95.00 max=100.00]");
    }

    #[test]
    fn summarize_handles_empty_window() {
        assert_eq!(summarize(&mut []), "[-]");
    }
}
