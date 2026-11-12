//! The shared clipboard. The KVM reads the clipboard for its copy action
//! and writes it for its paste action, while clipboard sharing is on in
//! its settings. Only text crosses, and content a password manager marks
//! as concealed is never read.

use crate::protocol::{
    ClipboardCapabilities, ClipboardReply, ClipboardStatus, ControlRecord, GuestOs,
    CLIPBOARD_MAX_TEXT,
};
use std::{
    sync::{mpsc, Mutex, MutexGuard, OnceLock},
    thread,
    time::Duration,
};

/// A clipboard call that hangs (an unresponsive X11 selection owner, a locked
/// Windows clipboard) must not stall the USB control loop. The request is
/// answered as failed and the worker is replaced, leaving the stuck call on
/// the abandoned thread so the next request starts clean.
const CALL_TIMEOUT: Duration = Duration::from_millis(1500);

/// Clipboard handles are not Send on every platform, and on X11 the pasted
/// text lives only as long as the handle that set it. One thread owns the
/// handle and serves every request until a call hangs and it is replaced.
enum Request {
    Probe(mpsc::Sender<bool>),
    Get(mpsc::Sender<GetResult>),
    Set(String, mpsc::Sender<ClipboardStatus>),
}

struct GetResult {
    status: ClipboardStatus,
    truncated: bool,
    text: String,
}

static WORKER: OnceLock<Mutex<mpsc::Sender<Request>>> = OnceLock::new();

/// Advertises access only when the clipboard opens, so a guest without one
/// (a Linux session whose compositor exposes no clipboard, say) sends no
/// capability and the KVM keeps its keystroke and screen-text fallbacks.
pub fn capabilities() -> ClipboardCapabilities {
    let available = call(Request::Probe, false);
    ClipboardCapabilities {
        read: available,
        write: available,
        os: guest_os(),
    }
}

fn guest_os() -> GuestOs {
    if cfg!(windows) {
        GuestOs::Windows
    } else if cfg!(target_os = "macos") {
        GuestOs::MacOs
    } else {
        GuestOs::Linux
    }
}

/// Answers a clipboard control record; any other record yields None. While
/// sharing is off on the KVM it sends no requests; one that arrives anyway
/// is answered Unsupported and no text leaves the guest.
pub fn answer(record: &ControlRecord, enabled: bool) -> Option<ClipboardReply> {
    match record {
        ControlRecord::ClipboardGet { request_id } => {
            let result = if enabled {
                call(Request::Get, without_text(ClipboardStatus::Failed))
            } else {
                without_text(ClipboardStatus::Unsupported)
            };
            Some(ClipboardReply::Content {
                request_id: *request_id,
                status: result.status,
                truncated: result.truncated,
                text: result.text,
            })
        }
        ControlRecord::ClipboardSet { request_id, text } => Some(ClipboardReply::Status {
            request_id: *request_id,
            status: if enabled {
                call(
                    |reply| Request::Set(text.clone(), reply),
                    ClipboardStatus::Failed,
                )
            } else {
                ClipboardStatus::Unsupported
            },
        }),
        _ => None,
    }
}

/// Runs one request on the worker and yields `fallback` when no answer
/// arrives in time, replacing the worker so the next request starts clean.
/// A worker that died takes the same path: its reply channel closes at once.
fn call<T>(request: impl FnOnce(mpsc::Sender<T>) -> Request, fallback: T) -> T {
    let worker = WORKER.get_or_init(|| Mutex::new(spawn()));
    let (reply, result) = mpsc::channel();
    let _ = lock(worker).send(request(reply));
    match result.recv_timeout(CALL_TIMEOUT) {
        Ok(value) => value,
        Err(_) => {
            eprintln!("ipkvm-os-services: clipboard worker did not answer; replacing it");
            *lock(worker) = spawn();
            fallback
        }
    }
}

fn lock(worker: &Mutex<mpsc::Sender<Request>>) -> MutexGuard<'_, mpsc::Sender<Request>> {
    worker
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn spawn() -> mpsc::Sender<Request> {
    let (tx, rx) = mpsc::channel();
    thread::Builder::new()
        .name("clipboard".into())
        .spawn(move || serve(rx))
        .expect("spawn clipboard thread");
    tx
}

fn serve(requests: mpsc::Receiver<Request>) {
    let mut clipboard: Option<arboard::Clipboard> = None;
    for request in requests {
        if clipboard.is_none() {
            clipboard = match arboard::Clipboard::new() {
                Ok(handle) => Some(handle),
                Err(err) => {
                    eprintln!("ipkvm-os-services: clipboard unavailable: {err}");
                    None
                }
            };
        }
        match request {
            Request::Probe(reply) => {
                let _ = reply.send(clipboard.is_some());
            }
            Request::Get(reply) => {
                let result = match clipboard.as_mut() {
                    Some(handle) => read(handle),
                    None => without_text(ClipboardStatus::Unsupported),
                };
                let _ = reply.send(result);
            }
            Request::Set(text, reply) => {
                let status = match clipboard.as_mut() {
                    Some(handle) => match handle.set_text(text) {
                        Ok(()) => ClipboardStatus::Ok,
                        Err(err) => {
                            eprintln!("ipkvm-os-services: clipboard write failed: {err}");
                            status_for(&err)
                        }
                    },
                    None => ClipboardStatus::Unsupported,
                };
                let _ = reply.send(status);
            }
        }
    }
}

/// What the concealed-content check found.
#[cfg_attr(any(target_os = "macos", windows), allow(dead_code))]
enum Checked {
    /// A password manager marked the content as secret.
    Secret,
    /// Not marked; arboard reads the text.
    Clear,
    /// Not marked, and the check read the text itself from the clipboard
    /// that it checked, or found none (Linux on Wayland).
    Read(Option<String>),
}

/// A clipboard whose mark cannot be checked is not read either.
fn read(clipboard: &mut arboard::Clipboard) -> GetResult {
    let read = match concealed() {
        Ok(Checked::Secret) => return without_text(ClipboardStatus::Denied),
        Ok(Checked::Clear) => clipboard.get_text(),
        Ok(Checked::Read(text)) => text.ok_or(arboard::Error::ContentNotAvailable),
        Err(err) => {
            eprintln!("ipkvm-os-services: clipboard types not listed: {err}");
            return without_text(ClipboardStatus::Failed);
        }
    };
    match read {
        Ok(text) if text.is_empty() => without_text(ClipboardStatus::Empty),
        Ok(text) => {
            let (text, truncated) = truncate(text);
            GetResult {
                status: ClipboardStatus::Ok,
                truncated,
                text,
            }
        }
        Err(arboard::Error::ContentNotAvailable) => without_text(ClipboardStatus::Empty),
        Err(err) => {
            eprintln!("ipkvm-os-services: clipboard read failed: {err}");
            without_text(status_for(&err))
        }
    }
}

fn without_text(status: ClipboardStatus) -> GetResult {
    GetResult {
        status,
        truncated: false,
        text: String::new(),
    }
}

/// Denied is reserved for concealed content, so every OS error, a locked
/// clipboard included, reports as Failed.
fn status_for(err: &arboard::Error) -> ClipboardStatus {
    match err {
        arboard::Error::ClipboardNotSupported => ClipboardStatus::Unsupported,
        _ => ClipboardStatus::Failed,
    }
}

/// Cuts at the last character boundary that fits the wire limit.
fn truncate(mut text: String) -> (String, bool) {
    let end = crate::protocol::truncate_utf8(&text, CLIPBOARD_MAX_TEXT).len();
    let truncated = end < text.len();
    text.truncate(end);
    (text, truncated)
}

/// Password managers flag secrets so clipboard tools leave them alone.
#[cfg(target_os = "macos")]
fn concealed() -> Result<Checked, Box<dyn std::error::Error>> {
    extern "C" {
        fn PasteboardIsConcealed() -> std::os::raw::c_int;
    }
    Ok(if unsafe { PasteboardIsConcealed() != 0 } {
        Checked::Secret
    } else {
        Checked::Clear
    })
}

#[cfg(windows)]
fn concealed() -> Result<Checked, Box<dyn std::error::Error>> {
    use winapi::um::winuser::{IsClipboardFormatAvailable, RegisterClipboardFormatW};
    let name: Vec<u16> = "ExcludeClipboardContentFromMonitorProcessing\0"
        .encode_utf16()
        .collect();
    let marked = unsafe {
        let format = RegisterClipboardFormatW(name.as_ptr());
        format != 0 && IsClipboardFormatAvailable(format) != 0
    };
    Ok(if marked {
        Checked::Secret
    } else {
        Checked::Clear
    })
}

#[cfg(not(any(target_os = "macos", windows)))]
mod linux;
#[cfg(not(any(target_os = "macos", windows)))]
use linux::concealed;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncates_on_character_boundary() {
        // A three-byte character never divides the limit evenly, so the cut
        // must step back to the previous boundary.
        let text = "€".repeat(CLIPBOARD_MAX_TEXT / 3 + 1);
        let (cut, truncated) = truncate(text);
        assert!(truncated);
        assert_eq!(cut.len(), CLIPBOARD_MAX_TEXT - CLIPBOARD_MAX_TEXT % 3);
        assert!(cut.chars().all(|c| c == '€'));

        let (kept, truncated) = truncate("short".to_owned());
        assert!(!truncated);
        assert_eq!(kept, "short");
    }

    #[test]
    fn refuses_requests_while_sharing_is_off() {
        assert_eq!(
            answer(&ControlRecord::ClipboardGet { request_id: 3 }, false),
            Some(ClipboardReply::Content {
                request_id: 3,
                status: ClipboardStatus::Unsupported,
                truncated: false,
                text: String::new(),
            })
        );
        assert_eq!(
            answer(
                &ControlRecord::ClipboardSet {
                    request_id: 4,
                    text: "secret".into(),
                },
                false,
            ),
            Some(ClipboardReply::Status {
                request_id: 4,
                status: ClipboardStatus::Unsupported,
            })
        );
    }
}
