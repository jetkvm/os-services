use std::fmt;

/// Why video cannot register yet. The session keeps its other services and
/// probes again.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CaptureFailure {
    /// No hardware H.264 encoder is available.
    #[cfg(not(target_os = "linux"))]
    EncoderUnavailable,
    /// The KMS capture pipeline would not open.
    #[cfg(target_os = "linux")]
    Kms(super::kms::KmsFailure),
    /// The installed capture helper is older than the one this build
    /// carries. Video stays unregistered until an administrator replaces
    /// it, so the KVM keeps HDMI capture.
    #[cfg(target_os = "linux")]
    HelperOlder,
    /// macOS has not granted this build Screen Recording. Video stays
    /// unregistered until it does, so the KVM keeps HDMI capture.
    #[cfg(target_os = "macos")]
    ScreenRecordingNotGranted,
}

impl fmt::Display for CaptureFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            #[cfg(not(target_os = "linux"))]
            Self::EncoderUnavailable => f.write_str("no hardware H.264 encoder is available"),
            #[cfg(target_os = "linux")]
            Self::Kms(failure) => failure.fmt(f),
            #[cfg(target_os = "linux")]
            Self::HelperOlder => f.write_str(
                "the installed capture helper is older than this service; update it in the \
                 dialog, or with the command in this log",
            ),
            #[cfg(target_os = "macos")]
            Self::ScreenRecordingNotGranted => {
                f.write_str("this build has no Screen Recording grant; if it is listed, re-add it")
            }
        }
    }
}

impl std::error::Error for CaptureFailure {}
