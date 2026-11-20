//! The zero-copy captures and encoders in `native/`: a
//! scanout grab through libdrmtap, encoded on the GPU that scans it out.
//! VAAPI imports and encodes it on Intel and AMD; on NVIDIA's proprietary
//! driver, EGL imports it and CUDA hands it to NVENC. A VAAPI driver without
//! an H.264 encoder converts the frame, and OpenH264 encodes it on the CPU.
//! When no GPU pipeline builds, the CPU path converts the frame as well.

use crate::protocol::VideoCodec;
use crate::video::display::{find_online, Display, DisplaySelector};
use std::ffi::{c_void, CStr, CString};
use std::fmt;
use std::os::raw::c_char;
use std::path::Path;
use std::sync::Mutex;

type PacketCallback = extern "C" fn(*const u8, i32, i32, u64, *const c_void);

extern "C" {
    fn kms_vaapi_new_encoder(
        device: *const c_char,
        connector_id: u32,
        encoder_name: *const c_char,
        framerate: i32,
        kbs: i32,
        gop: i32,
        failure: *mut i32,
    ) -> *mut c_void;
    fn kms_vaapi_dimensions(handle: *mut c_void, width: *mut i32, height: *mut i32) -> i32;
    fn kms_vaapi_set_rate(handle: *mut c_void, framerate: i32, kbs: i32) -> i32;
    fn kms_vaapi_next(
        handle: *mut c_void,
        callback: PacketCallback,
        obj: *const c_void,
        force_idr: i32,
    ) -> i32;
    fn kms_vaapi_encoder_name(handle: *mut c_void) -> *const c_char;
    fn kms_vaapi_destroy(handle: *mut c_void);

    fn kms_nvenc_new_encoder(
        device: *const c_char,
        connector_id: u32,
        encoder_name: *const c_char,
        framerate: i32,
        kbs: i32,
        gop: i32,
        failure: *mut i32,
    ) -> *mut c_void;
    fn kms_nvenc_dimensions(handle: *mut c_void, width: *mut i32, height: *mut i32) -> i32;
    fn kms_nvenc_set_rate(handle: *mut c_void, framerate: i32, kbs: i32) -> i32;
    fn kms_nvenc_next(
        handle: *mut c_void,
        callback: PacketCallback,
        obj: *const c_void,
        force_idr: i32,
    ) -> i32;
    fn kms_nvenc_encoder_name(handle: *mut c_void) -> *const c_char;
    fn kms_nvenc_destroy(handle: *mut c_void);

    fn kms_software_new_encoder(
        device: *const c_char,
        connector_id: u32,
        encoder_name: *const c_char,
        framerate: i32,
        kbs: i32,
        gop: i32,
        failure: *mut i32,
    ) -> *mut c_void;
    fn kms_software_dimensions(handle: *mut c_void, width: *mut i32, height: *mut i32) -> i32;
    fn kms_software_set_rate(handle: *mut c_void, framerate: i32, kbs: i32) -> i32;
    fn kms_software_next(
        handle: *mut c_void,
        callback: PacketCallback,
        obj: *const c_void,
        force_idr: i32,
    ) -> i32;
    fn kms_software_encoder_name(handle: *mut c_void) -> *const c_char;
    fn kms_software_destroy(handle: *mut c_void);
}

type NewEncoder =
    unsafe extern "C" fn(*const c_char, u32, *const c_char, i32, i32, i32, *mut i32) -> *mut c_void;

/// The C functions of one backend; every backend has the same signatures.
struct Functions {
    new_encoder: NewEncoder,
    dimensions: unsafe extern "C" fn(*mut c_void, *mut i32, *mut i32) -> i32,
    set_rate: unsafe extern "C" fn(*mut c_void, i32, i32) -> i32,
    next: unsafe extern "C" fn(*mut c_void, PacketCallback, *const c_void, i32) -> i32,
    encoder_name: unsafe extern "C" fn(*mut c_void) -> *const c_char,
    destroy: unsafe extern "C" fn(*mut c_void),
}

const VAAPI: Functions = Functions {
    new_encoder: kms_vaapi_new_encoder,
    dimensions: kms_vaapi_dimensions,
    set_rate: kms_vaapi_set_rate,
    next: kms_vaapi_next,
    encoder_name: kms_vaapi_encoder_name,
    destroy: kms_vaapi_destroy,
};

const NVENC: Functions = Functions {
    new_encoder: kms_nvenc_new_encoder,
    dimensions: kms_nvenc_dimensions,
    set_rate: kms_nvenc_set_rate,
    next: kms_nvenc_next,
    encoder_name: kms_nvenc_encoder_name,
    destroy: kms_nvenc_destroy,
};

/// The CPU path: libdrmtap brings the scanout to memory, and the CPU
/// converts it and encodes it with OpenH264.
const SOFTWARE: Functions = Functions {
    new_encoder: kms_software_new_encoder,
    dimensions: kms_software_dimensions,
    set_rate: kms_software_set_rate,
    next: kms_software_next,
    encoder_name: kms_software_encoder_name,
    destroy: kms_software_destroy,
};

/// The encoder family that takes a card's scanout. NVIDIA's proprietary
/// driver has no VAAPI encoder, so its cards encode with NVENC.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Backend {
    Vaapi,
    Nvenc,
}

impl Backend {
    /// The backend for a DRM card node such as /dev/dri/card1, from the
    /// kernel driver bound to the card's device.
    pub fn for_card(device: &str) -> Self {
        match card_driver(device).as_deref() {
            Some("nvidia" | "nvidia-drm") => Self::Nvenc,
            _ => Self::Vaapi,
        }
    }

    /// The FFmpeg encoder for `codec` that the pipeline tries first.
    pub fn encoder(self, codec: VideoCodec) -> &'static str {
        match (self, codec) {
            (Self::Vaapi, VideoCodec::H264) => "h264_vaapi",
            (Self::Vaapi, VideoCodec::H265) => "hevc_vaapi",
            (Self::Nvenc, VideoCodec::H264) => "h264_nvenc",
            (Self::Nvenc, VideoCodec::H265) => "hevc_nvenc",
        }
    }

    fn functions(self) -> &'static Functions {
        match self {
            Self::Vaapi => &VAAPI,
            Self::Nvenc => &NVENC,
        }
    }
}

/// The kernel driver bound to a DRM card's device: the last component of
/// /sys/class/drm/cardN/device/driver, a link to the driver.
fn card_driver(device: &str) -> Option<String> {
    let card = Path::new(device).file_name()?.to_str()?;
    let driver = std::fs::read_link(format!("/sys/class/drm/{card}/device/driver")).ok()?;
    Some(driver.file_name()?.to_string_lossy().into_owned())
}

/// Why the pipeline would not open. The values are `KmsOpenFailure` in
/// kms_capture.h.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KmsFailure {
    /// libdrmtap is not loadable, or not the version this was built against.
    LibraryUnavailable = 1,
    /// No DRM device, or it would not open.
    DeviceUnavailable = 2,
    /// drmtap-helper is missing, or this account may not run it. An install
    /// fixes it.
    HelperUnavailable = 3,
    /// No live framebuffer of a desktop: the display is off, re-plugging or
    /// between two modes. It is worth waiting out.
    NoScanout = 4,
    /// The encoder's GPU API cannot import the scanout format.
    FormatUnsupported = 5,
    /// The requested encoder would not open. A codec fallback answers it.
    EncoderUnavailable = 6,
    /// The grab, hardware context or filter graph failed; the log has why.
    PipelineFailed = 7,
    /// This account may not open the GPU's render node. Membership of the
    /// render group fixes it; without it, only the active desktop session's
    /// user has access.
    RenderNodeDenied = 8,
}

impl KmsFailure {
    fn from_code(code: i32) -> Self {
        match code {
            1 => Self::LibraryUnavailable,
            2 => Self::DeviceUnavailable,
            3 => Self::HelperUnavailable,
            4 => Self::NoScanout,
            5 => Self::FormatUnsupported,
            6 => Self::EncoderUnavailable,
            8 => Self::RenderNodeDenied,
            _ => Self::PipelineFailed,
        }
    }

    /// Whether the CPU path is worth a try after the GPU pipeline failed
    /// this way. It needs the same library, device, helper and scanout, so
    /// it fails the same way when one of them is missing. Every other
    /// failure is in the GPU's own stack, whatever the vendor or the cause,
    /// and the CPU path does without it. It encodes H.264 only.
    fn cpu_path_may_open(self, codec: VideoCodec) -> bool {
        codec == VideoCodec::H264
            && !matches!(
                self,
                Self::LibraryUnavailable
                    | Self::DeviceUnavailable
                    | Self::HelperUnavailable
                    | Self::NoScanout
            )
    }

    /// How much a failure says about the machine, for choosing which card's
    /// failure to report: shared library and helper problems first, then a
    /// scanout that can come back, then the most specific pipeline error.
    fn rank(self) -> u8 {
        match self {
            Self::LibraryUnavailable | Self::HelperUnavailable | Self::RenderNodeDenied => 5,
            Self::NoScanout => 4,
            Self::EncoderUnavailable => 3,
            Self::FormatUnsupported => 2,
            Self::PipelineFailed => 1,
            Self::DeviceUnavailable => 0,
        }
    }
}

impl fmt::Display for KmsFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::LibraryUnavailable => {
                "libdrmtap is not installed, or is not the version this service was built against"
            }
            Self::DeviceUnavailable => "no DRM device to capture from",
            Self::HelperUnavailable => {
                "drmtap-helper is not installed, or this account may not run it"
            }
            Self::NoScanout => "no display is being scanned out",
            Self::FormatUnsupported => {
                "the display's scanout format cannot be imported by the GPU's encoder"
            }
            Self::EncoderUnavailable => "the GPU's hardware encoder would not open",
            Self::PipelineFailed => "the capture pipeline failed to build; the log has why",
            Self::RenderNodeDenied => {
                "this account may not open the GPU's render node; add it to the render group"
            }
        })
    }
}

/// What one pull of the pipeline did. The values are `KmsPull` in
/// kms_capture.h.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pull {
    /// A frame went through; the callback saw every finished access unit.
    Encoded,
    /// The VAAPI filter graph took the frame and has no frame out yet, so
    /// nothing was encoded.
    Idle,
    /// The CPU path grabbed the scanout, and neither the picture nor the
    /// cursor had changed: nothing was encoded, and the capture is alive.
    Unchanged,
    /// The CPU path encoded a frame in which only the cursor changed.
    PointerOnly,
    /// The scanout has another size or layout: rebuild the capture for it.
    ScanoutChanged,
    /// The grab failed, as it does while the display is re-plugging.
    GrabFailed,
    /// The driver refused the rate `set_rate` gave the encoder: build a new
    /// encoder at that rate.
    RateRejected,
    /// The pipeline failed with this code.
    Failed(i32),
}

impl Pull {
    fn from_code(code: i32) -> Self {
        match code {
            0 => Self::Encoded,
            1 => Self::Idle,
            2 => Self::ScanoutChanged,
            3 => Self::RateRejected,
            4 => Self::Unchanged,
            5 => Self::PointerOnly,
            -1 => Self::GrabFailed,
            other => Self::Failed(other),
        }
    }
}

/// An open capture and encoder for one DRM output.
pub struct KmsEncoder {
    functions: &'static Functions,
    handle: *mut c_void,
    pub width: u32,
    pub height: u32,
}

// The handle is used only from the worker thread that owns the pipeline.
unsafe impl Send for KmsEncoder {}

struct CallbackState<'a> {
    callback: &'a mut dyn FnMut(&[u8], bool, u64),
}

extern "C" fn trampoline(data: *const u8, len: i32, key: i32, pts_us: u64, obj: *const c_void) {
    let state = unsafe { &mut *(obj as *mut CallbackState) };
    let unit = unsafe { std::slice::from_raw_parts(data, len as usize) };
    (state.callback)(unit, key != 0, pts_us);
}

impl KmsEncoder {
    /// Opens the GPU pipeline of `backend` and, when that fails for a reason
    /// the CPU path does not share, the CPU path. When both fail, the GPU
    /// pipeline's failure is the one reported.
    pub fn new(
        backend: Backend,
        device: &str,
        connector_id: u32,
        codec: VideoCodec,
        framerate: i32,
        kbps: i32,
        gop: i32,
    ) -> Result<Self, KmsFailure> {
        let open = |functions, encoder| {
            Self::open(
                functions,
                encoder,
                device,
                connector_id,
                framerate,
                kbps,
                gop,
            )
        };
        let gpu = open(backend.functions(), backend.encoder(codec));
        let gpu_failure = gpu.as_ref().err().copied();
        let opened = with_cpu_path(codec, gpu, || open(&SOFTWARE, "openh264_cpu"))?;
        if let Some(why) = gpu_failure {
            log_cpu_path(why);
        }
        Ok(opened)
    }

    fn open(
        functions: &'static Functions,
        encoder: &str,
        device: &str,
        connector_id: u32,
        framerate: i32,
        kbps: i32,
        gop: i32,
    ) -> Result<Self, KmsFailure> {
        let c_device = CString::new(device).expect("device path has no NUL");
        let c_encoder = CString::new(encoder).expect("encoder name has no NUL");
        let mut failure = 0i32;
        let handle = unsafe {
            (functions.new_encoder)(
                c_device.as_ptr(),
                connector_id,
                c_encoder.as_ptr(),
                framerate,
                kbps,
                gop,
                &mut failure,
            )
        };
        if handle.is_null() {
            return Err(KmsFailure::from_code(failure));
        }
        let (mut width, mut height) = (0i32, 0i32);
        unsafe { (functions.dimensions)(handle, &mut width, &mut height) };
        Ok(Self {
            functions,
            handle,
            width: width as u32,
            height: height as u32,
        })
    }

    /// The encoder the pipeline opened: `Backend::encoder`, openh264, or
    /// openh264_cpu on the CPU path.
    pub fn encoder_name(&self) -> String {
        unsafe { CStr::from_ptr((self.functions.encoder_name)(self.handle)) }
            .to_string_lossy()
            .into_owned()
    }

    /// Changes the frame rate and bitrate of the running encoder and keeps
    /// its stream. False when the encoder cannot change them in place.
    pub fn set_rate(&mut self, framerate: i32, kbps: i32) -> bool {
        unsafe { (self.functions.set_rate)(self.handle, framerate, kbps) == 0 }
    }

    /// Pulls one scanout frame and encodes it; `callback` receives each
    /// finished access unit with its keyframe flag and capture time.
    pub fn next(&mut self, force_idr: bool, mut callback: impl FnMut(&[u8], bool, u64)) -> Pull {
        let mut state = CallbackState {
            callback: &mut callback,
        };
        let code = unsafe {
            (self.functions.next)(
                self.handle,
                trampoline,
                &mut state as *mut CallbackState as *const c_void,
                force_idr as i32,
            )
        };
        Pull::from_code(code)
    }
}

impl Drop for KmsEncoder {
    fn drop(&mut self) {
        unsafe { (self.functions.destroy)(self.handle) }
    }
}

/// The GPU pipeline that `gpu` opened, or else the CPU path when the GPU's
/// failure allows it. When the CPU path fails too, the GPU's failure says
/// more about the machine, and is the one returned.
fn with_cpu_path<T>(
    codec: VideoCodec,
    gpu: Result<T, KmsFailure>,
    cpu: impl FnOnce() -> Result<T, KmsFailure>,
) -> Result<T, KmsFailure> {
    match gpu {
        Err(why) if why.cpu_path_may_open(codec) => cpu().map_err(|_| why),
        gpu => gpu,
    }
}

/// Says once per reason that video runs on the CPU path: the probes and
/// every reopen open it again.
fn log_cpu_path(why: KmsFailure) {
    static LOGGED: Mutex<Option<KmsFailure>> = Mutex::new(None);
    let mut logged = LOGGED
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if logged.replace(why) != Some(why) {
        eprintln!(
            "kms GPU pipeline did not build ({why}); video is encoded on the CPU, \
             with a higher CPU load and a lower frame rate"
        );
    }
}

/// Opens a small pipeline that only proves the machine can capture.
fn probe_encoder(card: &str, codec: VideoCodec) -> Result<KmsEncoder, KmsFailure> {
    KmsEncoder::new(Backend::for_card(card), card, 0, codec, 30, 2000, 30)
}

/// A DRM card the pipeline works on, and the H.264 encoder it opened.
pub struct Probed {
    pub card: String,
    pub encoder: String,
}

/// The DRM card the pipeline works on, or why none does. Opening a probe
/// builds the whole pipeline down to the encoder. The probe runs again
/// before each video registration: access to the GPU can change while the
/// process runs, and a pipeline that no longer builds must not be offered
/// to the KVM.
pub fn probe() -> Result<Probed, KmsFailure> {
    let mut reason = KmsFailure::DeviceUnavailable;
    for card in probe_order(capture_card()) {
        if !Path::new(&card).exists() {
            continue;
        }
        match probe_encoder(&card, VideoCodec::H264) {
            Ok(encoder) => {
                return Ok(Probed {
                    encoder: encoder.encoder_name(),
                    card,
                })
            }
            Err(why) if why.rank() > reason.rank() => reason = why,
            Err(_) => {}
        }
    }
    Err(reason)
}

/// The card of the display that capture selects when the user has picked
/// none: the display whose EDID the KVM presents. Capture and its encoder
/// run on the GPU of that card.
fn capture_card() -> Option<String> {
    let displays = Display::all().ok()?;
    let display = DisplaySelector::new(None).select(displays).ok()??;
    Some(display.device())
}

/// The cards to probe, in order: the card capture uses, when it is known,
/// then the cards a machine usually has.
fn probe_order(capture_card: Option<String>) -> Vec<String> {
    let mut cards: Vec<String> = capture_card.into_iter().collect();
    for card in ["/dev/dri/card1", "/dev/dri/card0", "/dev/dri/card2"] {
        if !cards.iter().any(|known| known == card) {
            cards.push(card.to_owned());
        }
    }
    cards
}

/// Whether to offer H.265: when its encoder opens, as on Windows and macOS.
/// The KVM asks for H.265 only when the browser needs it.
pub fn h265_enabled(card: &str) -> bool {
    probe_encoder(card, VideoCodec::H265).is_ok()
}

/// The DRM connector behind the display selected for capture, and the
/// backend its card encodes with.
pub struct CaptureTarget {
    pub device: String,
    pub connector: u32,
    pub backend: Backend,
}

impl CaptureTarget {
    pub fn for_display(name: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let display =
            find_online(name)?.ok_or_else(|| format!("capture display {name} is unavailable"))?;
        let device = display.device();
        Ok(Self {
            backend: Backend::for_card(&device),
            device,
            connector: display.connector(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{probe_order, with_cpu_path, KmsFailure, VideoCodec};

    /// The failures users met: a VAAPI driver that does not load, and one
    /// without video processing. Both leave the GPU pipeline unbuilt, and
    /// the CPU path takes over.
    #[test]
    fn a_gpu_pipeline_that_does_not_build_gives_way_to_the_cpu_path() {
        let opened = with_cpu_path(VideoCodec::H264, Err(KmsFailure::PipelineFailed), || {
            Ok("cpu")
        });
        assert_eq!(opened, Ok("cpu"));
        let opened = with_cpu_path(
            VideoCodec::H264,
            Err(KmsFailure::EncoderUnavailable),
            || Ok("cpu"),
        );
        assert_eq!(opened, Ok("cpu"));
    }

    /// The CPU path's own failure would hide why the GPU failed.
    #[test]
    fn when_the_cpu_path_fails_too_the_gpu_failure_is_reported() {
        let opened: Result<&str, _> =
            with_cpu_path(VideoCodec::H264, Err(KmsFailure::RenderNodeDenied), || {
                Err(KmsFailure::PipelineFailed)
            });
        assert_eq!(opened, Err(KmsFailure::RenderNodeDenied));
    }

    /// The CPU path needs the same library, device, helper and scanout, so
    /// it is not tried without them, and there is no H.265 encoder on the
    /// CPU. A working GPU pipeline is kept.
    #[test]
    fn the_cpu_path_is_not_tried_where_it_cannot_help() {
        for failure in [
            KmsFailure::LibraryUnavailable,
            KmsFailure::DeviceUnavailable,
            KmsFailure::HelperUnavailable,
            KmsFailure::NoScanout,
        ] {
            let opened: Result<&str, _> = with_cpu_path(VideoCodec::H264, Err(failure), || {
                panic!("CPU path tried after {failure:?}")
            });
            assert_eq!(opened, Err(failure));
        }
        let opened: Result<&str, _> =
            with_cpu_path(VideoCodec::H265, Err(KmsFailure::PipelineFailed), || {
                panic!("CPU path tried for H.265")
            });
        assert_eq!(opened, Err(KmsFailure::PipelineFailed));
        let opened = with_cpu_path(VideoCodec::H264, Ok("gpu"), || panic!("CPU path tried"));
        assert_eq!(opened, Ok("gpu"));
    }

    #[test]
    fn the_capture_card_is_probed_first_and_once() {
        assert_eq!(
            probe_order(Some("/dev/dri/card3".into())),
            [
                "/dev/dri/card3",
                "/dev/dri/card1",
                "/dev/dri/card0",
                "/dev/dri/card2"
            ]
        );
        assert_eq!(
            probe_order(Some("/dev/dri/card0".into())),
            ["/dev/dri/card0", "/dev/dri/card1", "/dev/dri/card2"]
        );
    }
}
