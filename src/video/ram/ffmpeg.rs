//! FFmpeg's hardware encoders on Windows and macOS (`encoder.c`), and the
//! probe that finds the ones this computer has.

use crate::video::nv12::Nv12Layout;
use std::ffi::{c_char, c_int, c_void, CString};
use std::{slice, time::Duration, time::Instant};

#[repr(C)]
struct RawEncoder {
    _private: [u8; 0],
}

type PacketFn = extern "C" fn(*mut c_void, *const u8, c_int, c_int);

extern "C" {
    fn ipkvm_encoder_open(
        name: *const c_char,
        width: c_int,
        height: c_int,
        fps: c_int,
        kbps: c_int,
        y_stride: *mut c_int,
        uv_offset: *mut c_int,
        uv_stride: *mut c_int,
    ) -> *mut RawEncoder;
    fn ipkvm_encoder_encode(
        encoder: *mut RawEncoder,
        nv12: *const u8,
        length: c_int,
        pts: i64,
        packet: PacketFn,
        context: *mut c_void,
    ) -> c_int;
    fn ipkvm_encoder_set_bitrate(encoder: *mut RawEncoder, kbps: c_int);
    fn ipkvm_encoder_request_keyframe(encoder: *mut RawEncoder);
    fn ipkvm_encoder_close(encoder: *mut RawEncoder);
}

/// One encoded packet: an access unit in Annex B.
pub struct Packet {
    pub data: Vec<u8>,
    pub key: bool,
}

/// An open hardware encoder for NV12 frames of one size.
pub struct Codec {
    raw: *mut RawEncoder,
    /// The NV12 layout that `encode` reads.
    pub layout: Nv12Layout,
    #[cfg(windows)]
    _timer: FineTimer,
}

impl Codec {
    /// Opens the FFmpeg encoder `name`, or None when this computer cannot.
    pub fn open(name: &str, (width, height): (usize, usize), fps: u64, kbps: u32) -> Option<Self> {
        let name = CString::new(name).ok()?;
        let (mut y_stride, mut uv_offset, mut uv_stride) = (0, 0, 0);
        let raw = unsafe {
            ipkvm_encoder_open(
                name.as_ptr(),
                width as c_int,
                height as c_int,
                fps as c_int,
                kbps as c_int,
                &mut y_stride,
                &mut uv_offset,
                &mut uv_stride,
            )
        };
        (!raw.is_null()).then(|| Self {
            raw,
            layout: Nv12Layout {
                width,
                height,
                y_stride: y_stride as usize,
                uv_offset: uv_offset as usize,
                uv_stride: uv_stride as usize,
            },
            #[cfg(windows)]
            _timer: FineTimer::start(),
        })
    }

    /// Encodes one NV12 frame and returns the packets the encoder has ready.
    pub fn encode(&mut self, nv12: &[u8], pts_ms: i64) -> Result<Vec<Packet>, i32> {
        extern "C" fn collect(context: *mut c_void, data: *const u8, size: c_int, key: c_int) {
            let packets = unsafe { &mut *(context as *mut Vec<Packet>) };
            packets.push(Packet {
                data: unsafe { slice::from_raw_parts(data, size as usize) }.to_vec(),
                key: key != 0,
            });
        }
        let mut packets = Vec::new();
        let result = unsafe {
            ipkvm_encoder_encode(
                self.raw,
                nv12.as_ptr(),
                nv12.len() as c_int,
                pts_ms,
                collect,
                &mut packets as *mut Vec<Packet> as *mut c_void,
            )
        };
        if result < 0 {
            return Err(result);
        }
        Ok(packets)
    }

    pub fn set_bitrate(&mut self, kbps: u32) {
        unsafe { ipkvm_encoder_set_bitrate(self.raw, kbps as c_int) };
    }

    /// Makes the next frame an IDR.
    pub fn request_keyframe(&mut self) {
        unsafe { ipkvm_encoder_request_keyframe(self.raw) };
    }
}

impl Drop for Codec {
    fn drop(&mut self) {
        unsafe { ipkvm_encoder_close(self.raw) };
    }
}

/// A 1 ms system timer while an encoder is open. AMF waits for its output
/// in sleeps that end on a timer tick, so at the default tick of 15.6 ms each
/// frame waits about 15 ms. Windows 11 ignores the timer requests of a
/// process that shows no window, as this service does, unless the process
/// opts out of that power saving.
#[cfg(windows)]
struct FineTimer;

#[cfg(windows)]
impl FineTimer {
    fn start() -> Self {
        use winapi::um::{processthreadsapi, timeapi};
        /// PROCESS_POWER_THROTTLING_STATE.
        #[repr(C)]
        struct PowerThrottling {
            version: u32,
            control_mask: u32,
            state_mask: u32,
        }
        const PROCESS_POWER_THROTTLING: u32 = 4;
        const IGNORE_TIMER_RESOLUTION: u32 = 4;
        // Controlled, and off: Windows honors the process's timer requests.
        let mut honor = PowerThrottling {
            version: 1,
            control_mask: IGNORE_TIMER_RESOLUTION,
            state_mask: 0,
        };
        unsafe {
            processthreadsapi::SetProcessInformation(
                processthreadsapi::GetCurrentProcess(),
                PROCESS_POWER_THROTTLING,
                (&raw mut honor).cast(),
                size_of::<PowerThrottling>() as u32,
            );
            timeapi::timeBeginPeriod(1);
        }
        Self
    }
}

#[cfg(windows)]
impl Drop for FineTimer {
    fn drop(&mut self) {
        unsafe { winapi::um::timeapi::timeEndPeriod(1) };
    }
}

/// The encoders to try, in the order of preference.
#[cfg(windows)]
const CANDIDATES: [&str; 6] = [
    "h264_qsv",
    "hevc_qsv",
    "h264_nvenc",
    "hevc_nvenc",
    "h264_amf",
    "hevc_amf",
];
#[cfg(target_os = "macos")]
const CANDIDATES: [&str; 2] = ["h264_videotoolbox", "hevc_videotoolbox"];

/// The first candidate whose name starts with `prefix` that opens and
/// encodes a test frame into one keyframe within a second.
pub fn first_encoder(prefix: &str) -> Option<String> {
    CANDIDATES
        .into_iter()
        .find(|name| name.starts_with(prefix) && encodes_a_keyframe(name))
        .map(str::to_owned)
}

fn encodes_a_keyframe(name: &str) -> bool {
    let Some(mut codec) = Codec::open(name, (1280, 720), 30, 1000) else {
        return false;
    };
    let frame = vec![0; codec.layout.buffer_len()];
    let started = Instant::now();
    let packets = codec.encode(&frame, 0);
    started.elapsed() < Duration::from_secs(1)
        && packets.is_ok_and(|packets| packets.len() == 1 && packets[0].key)
}
