//! The IP-KVM OS Services protocol as the sender speaks it: the records the
//! services exchange with the KVM, their wire encoding (`codec`), and the USB
//! link that carries them (`usb`). PROTOCOL.md is the specification.

pub mod codec;
pub mod identity;
mod usb;

pub use codec::truncate_utf8;
pub use identity::Fingerprint;
pub use usb::{attached_interfaces, find_and_negotiate, Transmit, UsbRx};

use std::{fmt, time::Duration};

/// The version the sender reports in session.info. A dev release sets
/// `IPKVM_OS_SERVICES_VERSION` at build time to the version it publishes,
/// such as 0.4.1-dev202609291830, as the firmware's dev release does; other
/// builds report the version in Cargo.toml.
pub const SENDER_VERSION: &str = match option_env!("IPKVM_OS_SERVICES_VERSION") {
    Some(version) if !version.is_empty() => version,
    _ => env!("CARGO_PKG_VERSION"),
};

/// The protocol version this sender speaks. The sender accepts a
/// session.ready only when it carries this version.
const PROTOCOL_MAJOR: u16 = 5;
const PROTOCOL_MINOR: u16 = 0;

const SESSION_SERVICE: u16 = 0;
pub(crate) const VIDEO_SERVICE: u16 = 1;
const CLIPBOARD_SERVICE: u16 = 2;
const TERMINAL_SERVICE: u16 = 3;
const FILE_SERVICE: u16 = 4;
const UPDATE_SERVICE: u16 = 5;

const SESSION_HELLO: u16 = 1;
const SESSION_READY: u16 = 2;
const SESSION_PING: u16 = 3;
const SESSION_CLOSE: u16 = 4;
const SESSION_CONFIG: u16 = 5;
const SESSION_INFO: u16 = 6;

const VIDEO_CAPABILITIES: u16 = 1;
const VIDEO_CONFIG: u16 = 3;
pub(crate) const VIDEO_STATUS: u16 = 4;
const VIDEO_FRAME: u16 = 5;
const VIDEO_REQUEST_KEYFRAME: u16 = 6;

const CLIPBOARD_CAPABILITIES: u16 = 1;
const CLIPBOARD_GET: u16 = 2;
const CLIPBOARD_CONTENT: u16 = 3;
const CLIPBOARD_SET: u16 = 4;
const CLIPBOARD_STATUS: u16 = 5;
const CLIPBOARD_CONFIG: u16 = 7;

const TERMINAL_CAPABILITIES: u16 = 1;
const TERMINAL_OPEN: u16 = 2;
const TERMINAL_OPENED: u16 = 3;
const TERMINAL_INPUT: u16 = 4;
const TERMINAL_OUTPUT: u16 = 5;
const TERMINAL_RESIZE: u16 = 6;
const TERMINAL_CLOSE: u16 = 7;
const TERMINAL_EXITED: u16 = 8;
const TERMINAL_CREDIT: u16 = 9;

const FILE_CAPABILITIES: u16 = 1;
const FILE_DROP: u16 = 2;
const FILE_READY: u16 = 3;
const FILE_CHUNK: u16 = 4;
const FILE_CREDIT: u16 = 5;
const FILE_DONE: u16 = 6;
const FILE_CANCEL: u16 = 7;

const UPDATE_CAPABILITIES: u16 = 1;
const UPDATE_BEGIN: u16 = 2;
const UPDATE_READY: u16 = 3;
const UPDATE_CHUNK: u16 = 4;
const UPDATE_CREDIT: u16 = 5;
const UPDATE_STATUS: u16 = 6;
const UPDATE_CANCEL: u16 = 7;

/// Every message starts with service, kind and payload length.
const ENVELOPE_LEN: usize = 8;
const MAX_CONTROL_PAYLOAD: usize = 64 << 10;
const MAX_FRAME_PAYLOAD: usize = 8 << 20;
/// The largest USB bulk transfer the sender submits.
const TRANSFER_CHUNK: usize = 64 << 10;

/// clipboard.content carries eight fixed bytes before the text; clipboard.set
/// shares the limit so one number describes both directions.
pub const CLIPBOARD_MAX_TEXT: usize = MAX_CONTROL_PAYLOAD - 8;
/// terminal.input and terminal.output carry the session id before the bytes.
pub const TERMINAL_MAX_DATA: usize = MAX_CONTROL_PAYLOAD - 4;
/// Shell output per terminal.output that still fits one USB transfer with
/// its envelope and session id.
pub const TERMINAL_OUTPUT_CHUNK: usize = TRANSFER_CHUNK - 12;
/// Shell output a terminal session may send beyond the credit the KVM
/// returned: what the KVM holds for a browser that has not taken it yet.
pub const TERMINAL_OUTPUT_WINDOW: usize = 128 << 10;
/// file.chunk carries twelve fixed bytes before the data.
pub const FILE_MAX_CHUNK: usize = MAX_CONTROL_PAYLOAD - 12;
/// update.chunk has the layout of file.chunk.
const UPDATE_MAX_CHUNK: usize = FILE_MAX_CHUNK;
/// The longest release version update.begin carries.
const UPDATE_MAX_VERSION: usize = 64;

const MAX_DIMENSION: u32 = 16384;
const MAX_FPS: u32 = 240;
const CAPTURE_MAX_WIDTH: u32 = 3840;
const CAPTURE_MAX_HEIGHT: u32 = 2160;
const CAPTURE_MAX_FPS: u32 = 120;

/// How long the KVM has to reset, answer the hello and send session.config.
const NEGOTIATION_TIMEOUT: Duration = Duration::from_secs(15);

/// Why the sender ends a session with session.close.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CloseReason {
    Shutdown = 0,
    /// The KVM's key is not in the user's approved list.
    NotApproved = 3,
}

/// The session ended because the user has not approved the KVM's key.
#[derive(Debug)]
pub struct NotApproved(pub Fingerprint);

impl fmt::Display for NotApproved {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "the KVM {} is not approved on this computer", self.0)
    }
}

impl std::error::Error for NotApproved {}

/// The sender found the KVM but may not open its USB device. On Linux this
/// is the missing USB access rule. Holds each device's name and USB ID.
#[derive(Debug)]
pub struct UsbAccessDenied(pub Vec<String>);

impl fmt::Display for UsbAccessDenied {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{HANDSHAKE_FAILED}: {}: {ACCESS_DENIED}",
            self.0.join(", ")
        )
    }
}

const HANDSHAKE_FAILED: &str = "no OS Services USB interface completed the handshake";

/// On Linux the USB access rule gives the user the KVM's device.
#[cfg(target_os = "linux")]
const ACCESS_DENIED: &str =
    "permission denied opening the KVM; install the USB access rule and reconnect it";
#[cfg(not(target_os = "linux"))]
const ACCESS_DENIED: &str = "permission denied opening the KVM";

impl std::error::Error for UsbAccessDenied {}

/// Disabled-status reason: the selected display is enumerated but inactive.
pub const VIDEO_DISPLAY_UNAVAILABLE: u32 = 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DisplayLayout {
    Mirror,
    Extend,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VideoCodec {
    H264,
    H265,
}

/// What the video service offers when it registers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VideoCapabilities {
    pub h265: bool,
    /// The sender sets the mode of the KVM display. Only for a KVM whose
    /// session.config has `display_mode_by_sender`: another KVM takes the
    /// flag for an error.
    pub sets_display_mode: bool,
    pub max_width: u32,
    pub max_height: u32,
    pub max_fps: u32,
}

impl VideoCapabilities {
    pub fn new(h265: bool) -> Self {
        Self {
            h265,
            sets_display_mode: false,
            max_width: CAPTURE_MAX_WIDTH,
            max_height: CAPTURE_MAX_HEIGHT,
            max_fps: CAPTURE_MAX_FPS,
        }
    }
}

/// The KVM's desired video state, from video.config.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VideoConfig {
    pub video_enabled: bool,
    pub layout: DisplayLayout,
    pub codec: VideoCodec,
    pub width: u32,
    pub height: u32,
    /// Zero means the encoder's default bitrate.
    pub target_kbps: u32,
    /// Zero means the native frame rate.
    pub maximum_fps: u32,
}

/// The sender's applied video state, sent as video.status.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VideoStatus {
    pub enabled: bool,
    pub layout: DisplayLayout,
    pub codec: VideoCodec,
    pub stream_reset: bool,
    pub actual_width: u32,
    pub actual_height: u32,
    pub native_fps: u32,
    pub default_kbps: u32,
    pub target_kbps: u32,
    pub maximum_fps: u32,
    pub error_code: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GuestOs {
    Windows = 1,
    MacOs = 2,
    Linux = 3,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ClipboardCapabilities {
    pub read: bool,
    pub write: bool,
    pub os: GuestOs,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClipboardStatus {
    Ok = 0,
    Unsupported = 1,
    Empty = 2,
    Denied = 3,
    Failed = 4,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ClipboardReply {
    Content {
        request_id: u32,
        status: ClipboardStatus,
        truncated: bool,
        text: String,
    },
    Status {
        request_id: u32,
        status: ClipboardStatus,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TerminalOpenStatus {
    Ok = 0,
    Unsupported = 1,
    Busy = 2,
    Failed = 3,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TerminalExitReason {
    Exited = 0,
    Closed = 1,
    Failed = 2,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TerminalSize {
    pub rows: u16,
    pub cols: u16,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TerminalReply {
    Capabilities {
        shell: bool,
    },
    Opened {
        session_id: u32,
        status: TerminalOpenStatus,
    },
    Output {
        session_id: u32,
        data: Vec<u8>,
    },
    Exited {
        session_id: u32,
        reason: TerminalExitReason,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FileCapabilities {
    pub receive: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileStatus {
    Ok = 0,
    Failed = 1,
    Cancelled = 2,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DroppedFile {
    pub transfer_id: u32,
    pub size: u64,
    pub name: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FileRecord {
    Drop {
        files: Vec<DroppedFile>,
    },
    Chunk {
        transfer_id: u32,
        offset: u64,
        data: Vec<u8>,
    },
    Cancel {
        transfer_id: u32,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FileReply {
    Ready {
        transfer_id: u32,
        credit: u32,
    },
    Credit {
        transfer_id: u32,
        bytes: u32,
    },
    Done {
        transfer_id: u32,
        status: FileStatus,
        path: String,
    },
}

/// The release package a sender can install, by its name on the wire.
/// Each build constructs only the package of its own OS.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UpdatePackage {
    WindowsX86_64,
    MacosAarch64,
    LinuxX86_64AppImage,
}

impl UpdatePackage {
    pub fn name(self) -> &'static str {
        match self {
            Self::WindowsX86_64 => "windows-x86_64",
            Self::MacosAarch64 => "macos-aarch64",
            Self::LinuxX86_64AppImage => "linux-x86_64-appimage",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UpdateStatus {
    Installing = 0,
    Failed = 1,
    Cancelled = 2,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UpdateRecord {
    Begin {
        update_id: u32,
        size: u64,
        sha256: [u8; 32],
        version: String,
    },
    Chunk {
        update_id: u32,
        offset: u64,
        data: Vec<u8>,
    },
    Cancel {
        update_id: u32,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UpdateReply {
    Ready {
        update_id: u32,
        credit: u32,
    },
    Credit {
        update_id: u32,
        bytes: u32,
    },
    Status {
        update_id: u32,
        status: UpdateStatus,
        message: String,
    },
}

/// One answer from the sender's services.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ServiceReply {
    SessionInfo { displays: Vec<String> },
    Clipboard(ClipboardReply),
    Terminal(TerminalReply),
    File(FileReply),
    Update(UpdateReply),
}

/// A service registering itself with the KVM, or withdrawing. A service
/// registers by sending its capabilities at any time after session.ready; a
/// repeated record re-registers, and an all-zero record withdraws.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Registration {
    Video(VideoCapabilities),
    VideoWithdrawn,
    Clipboard(ClipboardCapabilities),
    Terminal { shell: bool },
    File(FileCapabilities),
    Update(UpdatePackage),
}

/// The session policy from session.config.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize)]
pub struct SessionSettings {
    pub terminal_enabled: bool,
    pub files_enabled: bool,
    /// The display the user picked in the KVM's web UI; empty for automatic
    /// selection.
    pub capture_display: String,
    /// The KVM presents the same monitor each time to a sender that sets
    /// the display's mode and says so in video.capabilities.
    #[serde(default)]
    pub display_mode_by_sender: bool,
}

/// A record the KVM sends to the sender.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ControlRecord {
    KeyframeRequest,
    Config(VideoConfig),
    SessionConfig(SessionSettings),
    ClipboardGet {
        request_id: u32,
    },
    ClipboardSet {
        request_id: u32,
        text: String,
    },
    /// The KVM's clipboard-sharing setting: the answer to a clipboard
    /// registration, sent again on every change.
    ClipboardConfig {
        enabled: bool,
    },
    TerminalOpen {
        session_id: u32,
        size: TerminalSize,
    },
    TerminalInput {
        session_id: u32,
        data: Vec<u8>,
    },
    TerminalResize {
        session_id: u32,
        size: TerminalSize,
    },
    TerminalClose {
        session_id: u32,
    },
    /// The browser took this many more bytes of the session's output.
    TerminalCredit {
        session_id: u32,
        bytes: u32,
    },
    File(FileRecord),
    Update(UpdateRecord),
}

impl ControlRecord {
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::TerminalOpen { .. }
                | Self::TerminalInput { .. }
                | Self::TerminalResize { .. }
                | Self::TerminalClose { .. }
                | Self::TerminalCredit { .. }
        )
    }
}
