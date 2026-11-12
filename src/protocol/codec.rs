//! Byte layout of every message: the envelope, the payloads the sender
//! writes, and the records it reads. Pure functions; the USB link moves the
//! bytes.

use super::*;
use std::io;

/// One message: the envelope fields and the payload.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Message {
    pub service: u16,
    pub kind: u16,
    pub payload: Vec<u8>,
}

impl Message {
    fn new(service: u16, kind: u16, payload: Vec<u8>) -> Self {
        Self {
            service,
            kind,
            payload,
        }
    }

    /// The envelope and payload as one buffer.
    pub fn encode(&self) -> io::Result<Vec<u8>> {
        if self.payload.len() > MAX_CONTROL_PAYLOAD {
            return Err(invalid(format!(
                "control payload is too large: {}",
                self.payload.len()
            )));
        }
        let mut bytes = Vec::with_capacity(ENVELOPE_LEN + self.payload.len());
        bytes.extend_from_slice(&self.service.to_le_bytes());
        bytes.extend_from_slice(&self.kind.to_le_bytes());
        bytes.extend_from_slice(&(self.payload.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&self.payload);
        Ok(bytes)
    }
}

/// Removes the first complete message from `buffer`, or returns None while
/// the message is still arriving.
pub fn take_message(buffer: &mut Vec<u8>) -> io::Result<Option<Message>> {
    let Some(header) = buffer.get(..ENVELOPE_LEN) else {
        return Ok(None);
    };
    let mut fields = Fields::new(header);
    let (service, kind) = (fields.u16()?, fields.u16()?);
    let size = fields.u32()? as usize;
    if size > MAX_CONTROL_PAYLOAD {
        return Err(invalid(format!("KVM message payload is too large: {size}")));
    }
    if buffer.len() < ENVELOPE_LEN + size {
        return Ok(None);
    }
    let payload = buffer[ENVELOPE_LEN..ENVELOPE_LEN + size].to_vec();
    buffer.drain(..ENVELOPE_LEN + size);
    Ok(Some(Message::new(service, kind, payload)))
}

/// session.hello with the token from the reset and a fresh nonce, which
/// the KVM's signature in session.ready must cover.
pub fn hello(token: &[u8; 16], nonce: &[u8; 32]) -> Message {
    let payload = [&version_bytes()[..], token, nonce].concat();
    Message::new(SESSION_SERVICE, SESSION_HELLO, payload)
}

pub fn ping() -> Message {
    Message::new(SESSION_SERVICE, SESSION_PING, Vec::new())
}

pub fn close(reason: CloseReason) -> Message {
    Message::new(SESSION_SERVICE, SESSION_CLOSE, vec![reason as u8])
}

pub fn registration(registration: &Registration) -> io::Result<Message> {
    Ok(match registration {
        Registration::Video(capabilities) => video_capabilities(capabilities)?,
        Registration::VideoWithdrawn => {
            Message::new(VIDEO_SERVICE, VIDEO_CAPABILITIES, vec![0; 16])
        }
        Registration::Clipboard(capabilities) => {
            let flags = u32::from(capabilities.read) | u32::from(capabilities.write) << 1;
            let mut payload = flags.to_le_bytes().to_vec();
            payload.extend_from_slice(&[capabilities.os as u8, 0, 0, 0]);
            Message::new(CLIPBOARD_SERVICE, CLIPBOARD_CAPABILITIES, payload)
        }
        Registration::Terminal { shell } => {
            terminal_reply(&TerminalReply::Capabilities { shell: *shell })?
        }
        Registration::File(capabilities) => Message::new(
            FILE_SERVICE,
            FILE_CAPABILITIES,
            u32::from(capabilities.receive).to_le_bytes().to_vec(),
        ),
        Registration::Update(package) => {
            // Bit 0 is install, the one capability of the service.
            let mut payload = 1_u32.to_le_bytes().to_vec();
            payload.extend_from_slice(package.name().as_bytes());
            Message::new(UPDATE_SERVICE, UPDATE_CAPABILITIES, payload)
        }
    })
}

fn video_capabilities(capabilities: &VideoCapabilities) -> io::Result<Message> {
    validate_dimensions(capabilities.max_width, capabilities.max_height)?;
    validate_fps(capabilities.max_fps)?;
    // Bit 0 is H.264, which every sender supports; bit 1 is H.265.
    let codec_flags =
        1 | u32::from(capabilities.h265) << 1 | u32::from(capabilities.sets_display_mode) << 2;
    let mut payload = Vec::with_capacity(16);
    for value in [
        codec_flags,
        capabilities.max_width,
        capabilities.max_height,
        capabilities.max_fps,
    ] {
        payload.extend_from_slice(&value.to_le_bytes());
    }
    Ok(Message::new(VIDEO_SERVICE, VIDEO_CAPABILITIES, payload))
}

pub fn video_status(status: &VideoStatus) -> io::Result<Message> {
    if status.enabled {
        validate_dimensions(status.actual_width, status.actual_height)?;
        validate_fps(status.native_fps)?;
    }
    if status.maximum_fps > MAX_FPS {
        return Err(invalid("invalid video.status maximum_fps"));
    }
    let mut payload = vec![
        u8::from(status.enabled),
        layout_byte(status.layout),
        codec_byte(status.codec),
        u8::from(status.stream_reset),
    ];
    for value in [
        status.actual_width,
        status.actual_height,
        status.native_fps,
        status.default_kbps,
        status.target_kbps,
        status.maximum_fps,
        status.error_code,
    ] {
        payload.extend_from_slice(&value.to_le_bytes());
    }
    Ok(Message::new(VIDEO_SERVICE, VIDEO_STATUS, payload))
}

/// The first 20 bytes of a video.frame: the envelope, the capture time and
/// the flags. The access unit follows.
pub fn frame_header(
    unit_len: usize,
    capture_us: u64,
    keyframe: bool,
    parameter_sets: bool,
) -> io::Result<[u8; 20]> {
    let payload_len = unit_len + 12;
    if unit_len == 0 || payload_len > MAX_FRAME_PAYLOAD {
        return Err(invalid(format!(
            "invalid encoded access-unit size: {unit_len}"
        )));
    }
    let flags = u32::from(keyframe) | u32::from(parameter_sets) << 1;
    let mut header = [0; 20];
    header[0..2].copy_from_slice(&VIDEO_SERVICE.to_le_bytes());
    header[2..4].copy_from_slice(&VIDEO_FRAME.to_le_bytes());
    header[4..8].copy_from_slice(&(payload_len as u32).to_le_bytes());
    header[8..16].copy_from_slice(&capture_us.to_le_bytes());
    header[16..20].copy_from_slice(&flags.to_le_bytes());
    Ok(header)
}

pub fn service_reply(reply: &ServiceReply) -> io::Result<Message> {
    match reply {
        ServiceReply::SessionInfo { displays } => {
            let info = serde_json::json!({
                "version": SENDER_VERSION,
                "displays": displays,
            });
            Ok(Message::new(
                SESSION_SERVICE,
                SESSION_INFO,
                serde_json::to_vec(&info)?,
            ))
        }
        ServiceReply::Clipboard(reply) => clipboard_reply(reply),
        ServiceReply::Terminal(reply) => terminal_reply(reply),
        ServiceReply::File(reply) => Ok(file_reply(reply)),
        ServiceReply::Update(reply) => Ok(update_reply(reply)),
    }
}

fn clipboard_reply(reply: &ClipboardReply) -> io::Result<Message> {
    match reply {
        ClipboardReply::Content {
            request_id,
            status,
            truncated,
            text,
        } => {
            if text.len() > CLIPBOARD_MAX_TEXT {
                return Err(invalid(format!(
                    "clipboard text is too large: {}",
                    text.len()
                )));
            }
            let mut payload = request_id.to_le_bytes().to_vec();
            payload.extend_from_slice(&[*status as u8, u8::from(*truncated), 0, 0]);
            payload.extend_from_slice(text.as_bytes());
            Ok(Message::new(CLIPBOARD_SERVICE, CLIPBOARD_CONTENT, payload))
        }
        ClipboardReply::Status { request_id, status } => Ok(Message::new(
            CLIPBOARD_SERVICE,
            CLIPBOARD_STATUS,
            id_and_byte(*request_id, *status as u8),
        )),
    }
}

fn terminal_reply(reply: &TerminalReply) -> io::Result<Message> {
    let (kind, payload) = match reply {
        TerminalReply::Capabilities { shell } => (
            TERMINAL_CAPABILITIES,
            u32::from(*shell).to_le_bytes().to_vec(),
        ),
        TerminalReply::Opened { session_id, status } => {
            (TERMINAL_OPENED, id_and_byte(*session_id, *status as u8))
        }
        TerminalReply::Output { session_id, data } => {
            if data.is_empty() || data.len() > TERMINAL_MAX_DATA {
                return Err(invalid(format!(
                    "invalid terminal output size: {}",
                    data.len()
                )));
            }
            let mut payload = session_id.to_le_bytes().to_vec();
            payload.extend_from_slice(data);
            (TERMINAL_OUTPUT, payload)
        }
        TerminalReply::Exited { session_id, reason } => {
            (TERMINAL_EXITED, id_and_byte(*session_id, *reason as u8))
        }
    };
    Ok(Message::new(TERMINAL_SERVICE, kind, payload))
}

fn file_reply(reply: &FileReply) -> Message {
    let (kind, payload) = match reply {
        FileReply::Ready {
            transfer_id,
            credit,
        } => (FILE_READY, two_u32(*transfer_id, *credit)),
        FileReply::Credit { transfer_id, bytes } => (FILE_CREDIT, two_u32(*transfer_id, *bytes)),
        FileReply::Done {
            transfer_id,
            status,
            path,
        } => {
            // A path longer than the wire allows is cut: the file is written
            // either way, and the path is only shown to the user.
            let path = truncate_utf8(path, MAX_CONTROL_PAYLOAD - 8);
            let mut payload = transfer_id.to_le_bytes().to_vec();
            payload.extend_from_slice(&[*status as u8, 0, 0, 0]);
            payload.extend_from_slice(path.as_bytes());
            (FILE_DONE, payload)
        }
    };
    Message::new(FILE_SERVICE, kind, payload)
}

fn update_reply(reply: &UpdateReply) -> Message {
    let (kind, payload) = match reply {
        UpdateReply::Ready { update_id, credit } => (UPDATE_READY, two_u32(*update_id, *credit)),
        UpdateReply::Credit { update_id, bytes } => (UPDATE_CREDIT, two_u32(*update_id, *bytes)),
        UpdateReply::Status {
            update_id,
            status,
            message,
        } => {
            // The message is only shown to the user, so a long one is cut.
            let message = truncate_utf8(message, MAX_CONTROL_PAYLOAD - 8);
            let mut payload = update_id.to_le_bytes().to_vec();
            payload.extend_from_slice(&[*status as u8, 0, 0, 0]);
            payload.extend_from_slice(message.as_bytes());
            (UPDATE_STATUS, payload)
        }
    };
    Message::new(UPDATE_SERVICE, kind, payload)
}

fn id_and_byte(id: u32, byte: u8) -> Vec<u8> {
    let mut payload = id.to_le_bytes().to_vec();
    payload.push(byte);
    payload
}

fn two_u32(first: u32, second: u32) -> Vec<u8> {
    let mut payload = first.to_le_bytes().to_vec();
    payload.extend_from_slice(&second.to_le_bytes());
    payload
}

/// The longest prefix of `text` that fits `limit` bytes and ends on a
/// character boundary.
pub fn truncate_utf8(text: &str, limit: usize) -> &str {
    if text.len() <= limit {
        return text;
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// The record a message carries. Messages for other services and kinds this
/// sender does not know are skipped by their envelope and yield None.
pub fn decode(message: &Message) -> io::Result<Option<ControlRecord>> {
    let payload = message.payload.as_slice();
    let record = match (message.service, message.kind) {
        (VIDEO_SERVICE, VIDEO_REQUEST_KEYFRAME) => {
            if !payload.is_empty() {
                return Err(invalid("video.request_keyframe must have an empty payload"));
            }
            ControlRecord::KeyframeRequest
        }
        (VIDEO_SERVICE, VIDEO_CONFIG) => ControlRecord::Config(video_config(payload)?),
        (SESSION_SERVICE, SESSION_CONFIG) => ControlRecord::SessionConfig(
            serde_json::from_slice(payload).map_err(|err| invalid(err.to_string()))?,
        ),
        (CLIPBOARD_SERVICE, kind) => return clipboard_request(kind, payload),
        (TERMINAL_SERVICE, kind) => return terminal_request(kind, payload),
        (FILE_SERVICE, kind) => return Ok(file_record(kind, payload)?.map(ControlRecord::File)),
        (UPDATE_SERVICE, kind) => {
            return Ok(update_record(kind, payload)?.map(ControlRecord::Update))
        }
        _ => return Ok(None),
    };
    Ok(Some(record))
}

fn video_config(payload: &[u8]) -> io::Result<VideoConfig> {
    if payload.len() != 20 {
        return Err(invalid(format!(
            "invalid video.config payload length: {}",
            payload.len()
        )));
    }
    let mut fields = Fields::new(payload);
    let video_enabled = parse_bool(fields.u8()?, "video.config enabled")?;
    let layout = match fields.u8()? {
        0 => DisplayLayout::Mirror,
        1 => DisplayLayout::Extend,
        value => return Err(invalid(format!("invalid video layout: {value}"))),
    };
    let codec = match fields.u8()? {
        0 => VideoCodec::H264,
        1 => VideoCodec::H265,
        value => return Err(invalid(format!("invalid video codec: {value}"))),
    };
    if fields.u8()? != 0 {
        return Err(invalid("non-zero video.config reserved byte"));
    }
    let (width, height) = (fields.u32()?, fields.u32()?);
    validate_dimensions(width, height)?;
    let target_kbps = fields.u32()?;
    let maximum_fps = fields.u32()?;
    if maximum_fps > MAX_FPS {
        return Err(invalid(format!(
            "invalid maximum frame rate: {maximum_fps}"
        )));
    }
    Ok(VideoConfig {
        video_enabled,
        layout,
        codec,
        width,
        height,
        target_kbps,
        maximum_fps,
    })
}

fn clipboard_request(kind: u16, payload: &[u8]) -> io::Result<Option<ControlRecord>> {
    let record = match kind {
        CLIPBOARD_GET => {
            if payload.len() != 4 {
                return Err(invalid("invalid clipboard.get payload length"));
            }
            ControlRecord::ClipboardGet {
                request_id: Fields::new(payload).u32()?,
            }
        }
        CLIPBOARD_SET => {
            let mut fields = Fields::new(payload);
            let request_id = fields.u32()?;
            let text = fields.rest();
            if text.len() > CLIPBOARD_MAX_TEXT {
                return Err(invalid(format!(
                    "clipboard.set text is too large: {}",
                    text.len()
                )));
            }
            let text = std::str::from_utf8(text)
                .map_err(|_| invalid("clipboard.set text is not UTF-8"))?;
            ControlRecord::ClipboardSet {
                request_id,
                text: text.to_owned(),
            }
        }
        CLIPBOARD_CONFIG => match payload {
            [enabled] => ControlRecord::ClipboardConfig {
                enabled: parse_bool(*enabled, "clipboard.config enabled")?,
            },
            _ => return Err(invalid("invalid clipboard.config payload length")),
        },
        _ => return Ok(None),
    };
    Ok(Some(record))
}

/// Every terminal request starts with a non-zero session id; open and resize
/// carry a size, which resize requires to be non-zero because the shell has
/// no size of its own.
fn terminal_request(kind: u16, payload: &[u8]) -> io::Result<Option<ControlRecord>> {
    if !matches!(
        kind,
        TERMINAL_OPEN | TERMINAL_INPUT | TERMINAL_RESIZE | TERMINAL_CLOSE | TERMINAL_CREDIT
    ) {
        return Ok(None);
    }
    let mut fields = Fields::new(payload);
    let session_id = fields.u32()?;
    if session_id == 0 {
        return Err(invalid("terminal session id must be non-zero"));
    }
    let record = match kind {
        TERMINAL_OPEN => ControlRecord::TerminalOpen {
            session_id,
            size: terminal_size(fields)?,
        },
        TERMINAL_INPUT if !fields.is_empty() => ControlRecord::TerminalInput {
            session_id,
            data: fields.rest().to_vec(),
        },
        TERMINAL_INPUT => return Err(invalid("terminal.input carries no data")),
        TERMINAL_RESIZE => match terminal_size(fields)? {
            size if size.rows != 0 && size.cols != 0 => {
                ControlRecord::TerminalResize { session_id, size }
            }
            _ => return Err(invalid("terminal.resize needs non-zero rows and columns")),
        },
        TERMINAL_CREDIT => match (fields.u32()?, fields.is_empty()) {
            (bytes, true) => ControlRecord::TerminalCredit { session_id, bytes },
            _ => return Err(invalid("invalid terminal.credit payload length")),
        },
        _ if fields.is_empty() => ControlRecord::TerminalClose { session_id },
        _ => return Err(invalid("invalid terminal.close payload length")),
    };
    Ok(Some(record))
}

fn terminal_size(mut fields: Fields) -> io::Result<TerminalSize> {
    let size = TerminalSize {
        rows: fields.u16()?,
        cols: fields.u16()?,
    };
    fields.end()?;
    Ok(size)
}

fn file_record(kind: u16, payload: &[u8]) -> io::Result<Option<FileRecord>> {
    let mut fields = Fields::new(payload);
    let record = match kind {
        FILE_DROP => file_drop(fields)?,
        FILE_CHUNK => {
            if payload.len() > 12 + FILE_MAX_CHUNK {
                return Err(invalid("invalid file.chunk payload length"));
            }
            let transfer_id = fields.u32()?;
            let offset = fields.u64()?;
            if transfer_id == 0 {
                return Err(invalid("file.chunk without a transfer id"));
            }
            FileRecord::Chunk {
                transfer_id,
                offset,
                data: fields.rest().to_vec(),
            }
        }
        FILE_CANCEL => {
            let transfer_id = fields.u32()?;
            fields.end()?;
            FileRecord::Cancel { transfer_id }
        }
        _ => return Ok(None),
    };
    Ok(Some(record))
}

fn file_drop(mut fields: Fields) -> io::Result<FileRecord> {
    let count = fields.u32()? as usize;
    if count == 0 {
        return Err(invalid("file.drop names no files"));
    }
    let mut files = Vec::with_capacity(count.min(1024));
    for _ in 0..count {
        let transfer_id = fields.u32()?;
        let size = fields.u64()?;
        let name_len = fields.u16()? as usize;
        let name = std::str::from_utf8(fields.bytes(name_len)?)
            .map_err(|_| invalid("file.drop name is not UTF-8"))?;
        if transfer_id == 0 || !valid_file_name(name) {
            return Err(invalid("file.drop entry has an invalid id or name"));
        }
        files.push(DroppedFile {
            transfer_id,
            size,
            name: name.to_owned(),
        });
    }
    fields.end()?;
    Ok(FileRecord::Drop { files })
}

fn update_record(kind: u16, payload: &[u8]) -> io::Result<Option<UpdateRecord>> {
    let mut fields = Fields::new(payload);
    let record = match kind {
        UPDATE_BEGIN => {
            let update_id = fields.u32()?;
            let size = fields.u64()?;
            let sha256: [u8; 32] = fields.bytes(32)?.try_into().expect("32 bytes");
            let version = fields.rest();
            if update_id == 0 || size == 0 || version.len() > UPDATE_MAX_VERSION {
                return Err(invalid("update.begin has an invalid id, size or version"));
            }
            let version = std::str::from_utf8(version)
                .map_err(|_| invalid("update.begin version is not UTF-8"))?;
            UpdateRecord::Begin {
                update_id,
                size,
                sha256,
                version: version.to_owned(),
            }
        }
        UPDATE_CHUNK => {
            if payload.len() > 12 + UPDATE_MAX_CHUNK {
                return Err(invalid("invalid update.chunk payload length"));
            }
            let update_id = fields.u32()?;
            let offset = fields.u64()?;
            if update_id == 0 {
                return Err(invalid("update.chunk without an update id"));
            }
            UpdateRecord::Chunk {
                update_id,
                offset,
                data: fields.rest().to_vec(),
            }
        }
        UPDATE_CANCEL => {
            let update_id = fields.u32()?;
            fields.end()?;
            UpdateRecord::Cancel { update_id }
        }
        _ => return Ok(None),
    };
    Ok(Some(record))
}

/// A name is one path component: nothing that could leave the destination
/// folder and nothing a file system refuses outright.
fn valid_file_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name
            .chars()
            .any(|c| c == '/' || c == '\\' || c.is_control())
}

/// The session token from the answer to the vendor RESET or STATUS request,
/// with the reset phase: 0 in progress, 1 done, 2 in use, 3 stuck.
pub fn reset_status(payload: &[u8]) -> io::Result<(u8, [u8; 16])> {
    let mut fields = Fields::new(payload);
    let magic = fields.bytes(4)?;
    let phase = fields.u8()?;
    let reserved = fields.bytes(3)?;
    let token: [u8; 16] = fields.bytes(16)?.try_into().expect("16 bytes");
    fields.end()?;
    if magic != b"JKV5" || phase > 3 || reserved != [0; 3] {
        return Err(invalid("invalid KVM USB reset status"));
    }
    if token == [0; 16] {
        return Err(invalid("the KVM returned an empty session token"));
    }
    Ok((phase, token))
}

/// The start of the session.ready answering the hello with `token`, as it
/// appears on the wire: the envelope, the version and the token.
fn session_ready(token: &[u8; 16]) -> [u8; ENVELOPE_LEN + 20] {
    let mut ready = [0; ENVELOPE_LEN + 20];
    ready[0..2].copy_from_slice(&SESSION_SERVICE.to_le_bytes());
    ready[2..4].copy_from_slice(&SESSION_READY.to_le_bytes());
    ready[4..8].copy_from_slice(&(identity::READY_PAYLOAD_LEN as u32).to_le_bytes());
    ready[8..12].copy_from_slice(&version_bytes());
    ready[12..].copy_from_slice(token);
    ready
}

/// Where the session.ready for `token` starts in `bytes`. The KVM's reply
/// to an earlier session can precede it, so the stream is searched rather
/// than parsed until the new session is synchronized.
pub fn ready_offset(bytes: &[u8], token: &[u8; 16]) -> Option<usize> {
    let expected = session_ready(token);
    bytes
        .windows(expected.len())
        .position(|window| window == expected)
}

/// Bytes kept while searching for session.ready: a prefix of it can end
/// the buffer.
pub const READY_SEARCH_KEEP: usize = ENVELOPE_LEN + 19;

fn version_bytes() -> [u8; 4] {
    let mut bytes = [0; 4];
    bytes[..2].copy_from_slice(&PROTOCOL_MAJOR.to_le_bytes());
    bytes[2..].copy_from_slice(&PROTOCOL_MINOR.to_le_bytes());
    bytes
}

fn layout_byte(layout: DisplayLayout) -> u8 {
    match layout {
        DisplayLayout::Mirror => 0,
        DisplayLayout::Extend => 1,
    }
}

fn codec_byte(codec: VideoCodec) -> u8 {
    match codec {
        VideoCodec::H264 => 0,
        VideoCodec::H265 => 1,
    }
}

fn parse_bool(value: u8, field: &str) -> io::Result<bool> {
    match value {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(invalid(format!("invalid {field} value: {value}"))),
    }
}

fn validate_dimensions(width: u32, height: u32) -> io::Result<()> {
    if width == 0 || width > MAX_DIMENSION || height == 0 || height > MAX_DIMENSION {
        return Err(invalid(format!(
            "invalid display resolution: {width}x{height}"
        )));
    }
    Ok(())
}

fn validate_fps(fps: u32) -> io::Result<()> {
    if fps == 0 || fps > MAX_FPS {
        return Err(invalid(format!("invalid frame rate: {fps}")));
    }
    Ok(())
}

pub fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

/// Reads little-endian fields from the front of a payload.
struct Fields<'a> {
    bytes: &'a [u8],
}

impl<'a> Fields<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes }
    }

    fn bytes(&mut self, len: usize) -> io::Result<&'a [u8]> {
        if self.bytes.len() < len {
            return Err(invalid("message payload is truncated"));
        }
        let (field, rest) = self.bytes.split_at(len);
        self.bytes = rest;
        Ok(field)
    }

    fn u8(&mut self) -> io::Result<u8> {
        Ok(self.bytes(1)?[0])
    }

    fn u16(&mut self) -> io::Result<u16> {
        Ok(u16::from_le_bytes(
            self.bytes(2)?.try_into().expect("2 bytes"),
        ))
    }

    fn u32(&mut self) -> io::Result<u32> {
        Ok(u32::from_le_bytes(
            self.bytes(4)?.try_into().expect("4 bytes"),
        ))
    }

    fn u64(&mut self) -> io::Result<u64> {
        Ok(u64::from_le_bytes(
            self.bytes(8)?.try_into().expect("8 bytes"),
        ))
    }

    fn rest(&mut self) -> &'a [u8] {
        std::mem::take(&mut self.bytes)
    }

    fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    fn end(self) -> io::Result<()> {
        if !self.bytes.is_empty() {
            return Err(invalid("message payload has trailing bytes"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(service: u16, kind: u16, payload: &[u8]) -> Message {
        Message::new(service, kind, payload.to_vec())
    }

    fn record(service: u16, kind: u16, payload: &[u8]) -> io::Result<Option<ControlRecord>> {
        decode(&message(service, kind, payload))
    }

    fn config_payload() -> Vec<u8> {
        let mut payload = vec![1, 0, 0, 0];
        for value in [2560_u32, 1440, 8_000, 60] {
            payload.extend_from_slice(&value.to_le_bytes());
        }
        payload
    }

    #[test]
    fn envelope_round_trips_and_waits_for_the_whole_message() {
        let bytes = message(TERMINAL_SERVICE, TERMINAL_OUTPUT, b"abc")
            .encode()
            .unwrap();
        assert_eq!(bytes, [3, 0, 5, 0, 3, 0, 0, 0, b'a', b'b', b'c']);
        let mut buffer = bytes[..10].to_vec();
        assert_eq!(take_message(&mut buffer).unwrap(), None);
        buffer.extend_from_slice(&bytes[10..]);
        buffer.extend_from_slice(&[0, 0]);
        assert_eq!(
            take_message(&mut buffer).unwrap(),
            Some(message(TERMINAL_SERVICE, TERMINAL_OUTPUT, b"abc"))
        );
        assert_eq!(buffer, [0, 0], "the next message's bytes stay buffered");
    }

    #[test]
    fn envelope_rejects_oversized_payloads() {
        let mut header = vec![3, 0, 5, 0];
        header.extend_from_slice(&(MAX_CONTROL_PAYLOAD as u32 + 1).to_le_bytes());
        assert!(take_message(&mut header).is_err());
        let oversized = message(
            SESSION_SERVICE,
            SESSION_INFO,
            &vec![0; MAX_CONTROL_PAYLOAD + 1],
        );
        assert!(oversized.encode().is_err());
    }

    #[test]
    fn writes_video_status_in_specified_field_order() {
        let status = VideoStatus {
            enabled: true,
            layout: DisplayLayout::Extend,
            codec: VideoCodec::H265,
            stream_reset: true,
            actual_width: 1920,
            actual_height: 1080,
            native_fps: 120,
            default_kbps: 9_000,
            target_kbps: 4_000,
            maximum_fps: 60,
            error_code: 0,
        };
        let payload = video_status(&status).unwrap().payload;
        assert_eq!(&payload[..4], [1, 1, 1, 1]);
        let mut fields = Fields::new(&payload[4..]);
        let values: Vec<u32> = (0..7).map(|_| fields.u32().unwrap()).collect();
        assert_eq!(values, [1920, 1080, 120, 9_000, 4_000, 60, 0]);
        fields.end().unwrap();
    }

    #[test]
    fn a_disabled_status_needs_no_mode() {
        let status = VideoStatus {
            enabled: false,
            layout: DisplayLayout::Mirror,
            codec: VideoCodec::H264,
            stream_reset: true,
            actual_width: 0,
            actual_height: 0,
            native_fps: 0,
            default_kbps: 0,
            target_kbps: 0,
            maximum_fps: 0,
            error_code: VIDEO_DISPLAY_UNAVAILABLE,
        };
        assert_eq!(video_status(&status).unwrap().payload.len(), 32);
        let enabled = VideoStatus {
            enabled: true,
            ..status
        };
        assert!(video_status(&enabled).is_err());
    }

    #[test]
    fn only_a_kvm_that_says_so_leaves_the_display_mode_to_the_sender() {
        let config = |json: &str| match record(SESSION_SERVICE, SESSION_CONFIG, json.as_bytes()) {
            Ok(Some(ControlRecord::SessionConfig(settings))) => settings,
            other => panic!("not a session.config: {other:?}"),
        };
        let policy = r#""terminal_enabled":true,"files_enabled":false,"capture_display":"""#;
        assert!(!config(&format!("{{{policy}}}")).display_mode_by_sender);
        assert!(
            config(&format!(r#"{{{policy},"display_mode_by_sender":true}}"#))
                .display_mode_by_sender
        );
    }

    #[test]
    fn writes_registrations() {
        let video = registration(&Registration::Video(VideoCapabilities::new(true))).unwrap();
        assert_eq!(
            (video.service, video.kind),
            (VIDEO_SERVICE, VIDEO_CAPABILITIES)
        );
        assert_eq!(&video.payload[..4], 3_u32.to_le_bytes());
        let sets_mode = registration(&Registration::Video(VideoCapabilities {
            sets_display_mode: true,
            ..VideoCapabilities::new(false)
        }))
        .unwrap();
        assert_eq!(&sets_mode.payload[..4], 5_u32.to_le_bytes());
        let withdrawn = registration(&Registration::VideoWithdrawn).unwrap();
        assert_eq!(withdrawn.payload, [0; 16]);
        let clipboard = registration(&Registration::Clipboard(ClipboardCapabilities {
            read: true,
            write: false,
            os: GuestOs::Linux,
        }))
        .unwrap();
        assert_eq!(clipboard.payload, [1, 0, 0, 0, 3, 0, 0, 0]);
        let files = registration(&Registration::File(FileCapabilities { receive: true })).unwrap();
        assert_eq!(files.payload, [1, 0, 0, 0]);
        let update = registration(&Registration::Update(UpdatePackage::MacosAarch64)).unwrap();
        assert_eq!(
            (update.service, update.kind),
            (UPDATE_SERVICE, UPDATE_CAPABILITIES)
        );
        assert_eq!(&update.payload[..4], [1, 0, 0, 0]);
        assert_eq!(&update.payload[4..], b"macos-aarch64");
    }

    #[test]
    fn frame_header_carries_size_time_and_flags() {
        let header = frame_header(100, 7, true, false).unwrap();
        assert_eq!(&header[..4], [1, 0, 5, 0]);
        assert_eq!(u32::from_le_bytes(header[4..8].try_into().unwrap()), 112);
        assert_eq!(u64::from_le_bytes(header[8..16].try_into().unwrap()), 7);
        assert_eq!(u32::from_le_bytes(header[16..20].try_into().unwrap()), 1);
        assert!(frame_header(0, 0, false, false).is_err());
        assert!(frame_header(MAX_FRAME_PAYLOAD, 0, false, false).is_err());
    }

    #[test]
    fn writes_service_replies() {
        let done = service_reply(&ServiceReply::File(FileReply::Done {
            transfer_id: 9,
            status: FileStatus::Ok,
            path: "/tmp/a".into(),
        }))
        .unwrap();
        assert_eq!(
            done.payload,
            [9, 0, 0, 0, 0, 0, 0, 0, b'/', b't', b'm', b'p', b'/', b'a']
        );
        let opened = service_reply(&ServiceReply::Terminal(TerminalReply::Opened {
            session_id: 2,
            status: TerminalOpenStatus::Busy,
        }))
        .unwrap();
        assert_eq!(opened.payload, [2, 0, 0, 0, 2]);
        let empty_output = ServiceReply::Terminal(TerminalReply::Output {
            session_id: 2,
            data: Vec::new(),
        });
        assert!(service_reply(&empty_output).is_err());
        let content = service_reply(&ServiceReply::Clipboard(ClipboardReply::Content {
            request_id: 5,
            status: ClipboardStatus::Ok,
            truncated: true,
            text: "hi".into(),
        }))
        .unwrap();
        assert_eq!(content.payload, [5, 0, 0, 0, 0, 1, 0, 0, b'h', b'i']);
        let failed = service_reply(&ServiceReply::Update(UpdateReply::Status {
            update_id: 3,
            status: UpdateStatus::Failed,
            message: "no".into(),
        }))
        .unwrap();
        assert_eq!(
            (failed.service, failed.kind),
            (UPDATE_SERVICE, UPDATE_STATUS)
        );
        assert_eq!(failed.payload, [3, 0, 0, 0, 1, 0, 0, 0, b'n', b'o']);
    }

    #[test]
    fn reads_clipboard_requests() {
        let mut set = 7_u32.to_le_bytes().to_vec();
        set.extend_from_slice("héllo".as_bytes());
        assert_eq!(
            record(CLIPBOARD_SERVICE, CLIPBOARD_SET, &set).unwrap(),
            Some(ControlRecord::ClipboardSet {
                request_id: 7,
                text: "héllo".to_owned(),
            })
        );
        assert!(record(CLIPBOARD_SERVICE, CLIPBOARD_SET, &[1, 2, 3]).is_err());
        let mut broken = 1_u32.to_le_bytes().to_vec();
        broken.extend_from_slice(&[0xff, 0xfe]);
        assert!(record(CLIPBOARD_SERVICE, CLIPBOARD_SET, &broken).is_err());
        let mut oversized = 1_u32.to_le_bytes().to_vec();
        oversized.resize(4 + CLIPBOARD_MAX_TEXT + 1, b'a');
        assert!(record(CLIPBOARD_SERVICE, CLIPBOARD_SET, &oversized).is_err());

        assert_eq!(
            record(CLIPBOARD_SERVICE, CLIPBOARD_CONFIG, &[1]).unwrap(),
            Some(ControlRecord::ClipboardConfig { enabled: true })
        );
        for bad in [&[][..], &[2], &[1, 0]] {
            assert!(record(CLIPBOARD_SERVICE, CLIPBOARD_CONFIG, bad).is_err());
        }
        assert_eq!(
            record(CLIPBOARD_SERVICE, CLIPBOARD_GET, &3_u32.to_le_bytes()).unwrap(),
            Some(ControlRecord::ClipboardGet { request_id: 3 })
        );
    }

    #[test]
    fn reads_terminal_requests() {
        let with = |bytes: &[u8]| {
            let mut payload = 9_u32.to_le_bytes().to_vec();
            payload.extend_from_slice(bytes);
            payload
        };
        assert_eq!(
            record(TERMINAL_SERVICE, TERMINAL_OPEN, &with(&[0, 0, 0, 0])).unwrap(),
            Some(ControlRecord::TerminalOpen {
                session_id: 9,
                size: TerminalSize { rows: 0, cols: 0 },
            })
        );
        assert_eq!(
            record(TERMINAL_SERVICE, TERMINAL_INPUT, &with(b"ls\r")).unwrap(),
            Some(ControlRecord::TerminalInput {
                session_id: 9,
                data: b"ls\r".to_vec(),
            })
        );
        assert_eq!(
            record(TERMINAL_SERVICE, TERMINAL_RESIZE, &with(&[40, 0, 132, 0])).unwrap(),
            Some(ControlRecord::TerminalResize {
                session_id: 9,
                size: TerminalSize {
                    rows: 40,
                    cols: 132
                },
            })
        );
        assert_eq!(
            record(TERMINAL_SERVICE, TERMINAL_CLOSE, &with(&[])).unwrap(),
            Some(ControlRecord::TerminalClose { session_id: 9 })
        );
        assert_eq!(
            record(TERMINAL_SERVICE, TERMINAL_CREDIT, &with(&[0, 64, 0, 0])).unwrap(),
            Some(ControlRecord::TerminalCredit {
                session_id: 9,
                bytes: 16384
            })
        );
        // A kind this sender does not know is skipped, whatever it carries.
        assert_eq!(record(TERMINAL_SERVICE, 99, &[]).unwrap(), None);

        assert!(record(TERMINAL_SERVICE, TERMINAL_OPEN, &[1, 2]).is_err());
        assert!(record(TERMINAL_SERVICE, TERMINAL_OPEN, &[0; 8]).is_err());
        assert!(record(TERMINAL_SERVICE, TERMINAL_INPUT, &with(&[])).is_err());
        assert!(record(TERMINAL_SERVICE, TERMINAL_RESIZE, &with(&[0; 4])).is_err());
        assert!(record(TERMINAL_SERVICE, TERMINAL_CLOSE, &with(&[0; 4])).is_err());
        assert!(record(TERMINAL_SERVICE, TERMINAL_CREDIT, &with(&[0; 2])).is_err());
        assert!(record(TERMINAL_SERVICE, TERMINAL_CREDIT, &with(&[0; 5])).is_err());
    }

    #[test]
    fn reads_file_drop() {
        let mut payload = 2_u32.to_le_bytes().to_vec();
        for (id, name) in [(7_u32, "a.txt"), (8, "résumé.pdf")] {
            payload.extend_from_slice(&id.to_le_bytes());
            payload.extend_from_slice(&1234_u64.to_le_bytes());
            payload.extend_from_slice(&(name.len() as u16).to_le_bytes());
            payload.extend_from_slice(name.as_bytes());
        }
        let file = |transfer_id, name: &str| DroppedFile {
            transfer_id,
            size: 1234,
            name: name.to_owned(),
        };
        assert_eq!(
            file_record(FILE_DROP, &payload).unwrap(),
            Some(FileRecord::Drop {
                files: vec![file(7, "a.txt"), file(8, "résumé.pdf")],
            })
        );
        assert!(file_record(FILE_DROP, &payload[..payload.len() - 1]).is_err());
        let mut trailing = payload.clone();
        trailing.push(0);
        assert!(file_record(FILE_DROP, &trailing).is_err());
        let mut escaped = payload.clone();
        let at = escaped.len() - "résumé.pdf".len();
        escaped[at] = b'/';
        assert!(file_record(FILE_DROP, &escaped).is_err());
    }

    #[test]
    fn reads_file_chunk_and_cancel() {
        let mut payload = 9_u32.to_le_bytes().to_vec();
        payload.extend_from_slice(&65536_u64.to_le_bytes());
        payload.extend_from_slice(b"data");
        assert_eq!(
            file_record(FILE_CHUNK, &payload).unwrap(),
            Some(FileRecord::Chunk {
                transfer_id: 9,
                offset: 65536,
                data: b"data".to_vec(),
            })
        );
        assert!(file_record(FILE_CHUNK, &payload[..11]).is_err());
        assert_eq!(
            file_record(FILE_CANCEL, &9_u32.to_le_bytes()).unwrap(),
            Some(FileRecord::Cancel { transfer_id: 9 })
        );
        assert!(file_record(FILE_CANCEL, &payload).is_err());
        assert_eq!(file_record(99, &[]).unwrap(), None);
    }

    fn update_begin(update_id: u32, size: u64, version: &[u8]) -> Vec<u8> {
        let mut payload = update_id.to_le_bytes().to_vec();
        payload.extend_from_slice(&size.to_le_bytes());
        payload.extend_from_slice(&[0xab; 32]);
        payload.extend_from_slice(version);
        payload
    }

    #[test]
    fn reads_update_begin() {
        assert_eq!(
            record(
                UPDATE_SERVICE,
                UPDATE_BEGIN,
                &update_begin(4, 1000, b"0.5.0")
            )
            .unwrap(),
            Some(ControlRecord::Update(UpdateRecord::Begin {
                update_id: 4,
                size: 1000,
                sha256: [0xab; 32],
                version: "0.5.0".into(),
            }))
        );
        for bad in [
            update_begin(0, 1000, b"0.5.0"),
            update_begin(4, 0, b"0.5.0"),
            update_begin(4, 1000, &[b'1'; UPDATE_MAX_VERSION + 1]),
            update_begin(4, 1000, &[0xff]),
            update_begin(4, 1000, b"")[..43].to_vec(),
        ] {
            assert!(
                update_record(UPDATE_BEGIN, &bad).is_err(),
                "{bad:?} accepted"
            );
        }
    }

    #[test]
    fn reads_update_chunk_and_cancel() {
        let mut payload = 4_u32.to_le_bytes().to_vec();
        payload.extend_from_slice(&8_u64.to_le_bytes());
        payload.extend_from_slice(b"data");
        assert_eq!(
            update_record(UPDATE_CHUNK, &payload).unwrap(),
            Some(UpdateRecord::Chunk {
                update_id: 4,
                offset: 8,
                data: b"data".to_vec(),
            })
        );
        payload[..4].copy_from_slice(&[0; 4]);
        assert!(update_record(UPDATE_CHUNK, &payload).is_err());
        assert_eq!(
            update_record(UPDATE_CANCEL, &4_u32.to_le_bytes()).unwrap(),
            Some(UpdateRecord::Cancel { update_id: 4 })
        );
        assert!(update_record(UPDATE_CANCEL, &[4, 0, 0, 0, 0]).is_err());
        assert_eq!(update_record(99, &[]).unwrap(), None);
    }

    #[test]
    fn validates_file_names() {
        assert!(valid_file_name("report.pdf"));
        assert!(valid_file_name(".hidden"));
        for bad in ["", ".", "..", "a/b", "a\\b", "tab\tname"] {
            assert!(!valid_file_name(bad), "{bad:?} accepted");
        }
    }

    #[test]
    fn reads_video_config() {
        assert_eq!(
            video_config(&config_payload()).unwrap(),
            VideoConfig {
                video_enabled: true,
                layout: DisplayLayout::Mirror,
                codec: VideoCodec::H264,
                width: 2560,
                height: 1440,
                target_kbps: 8_000,
                maximum_fps: 60,
            }
        );
        let mut disabled_h265 = config_payload();
        disabled_h265[..3].copy_from_slice(&[0, 1, 1]);
        let config = video_config(&disabled_h265).unwrap();
        assert_eq!(
            (config.video_enabled, config.layout, config.codec),
            (false, DisplayLayout::Extend, VideoCodec::H265)
        );
    }

    #[test]
    fn rejects_invalid_video_configs() {
        for (index, value) in [(0, 2), (1, 2), (2, 9), (3, 1)] {
            let mut payload = config_payload();
            payload[index] = value;
            assert!(video_config(&payload).is_err());
        }
        let mut zero_width = config_payload();
        zero_width[4..8].fill(0);
        assert!(video_config(&zero_width).is_err());
        let mut fast = config_payload();
        fast[16..20].copy_from_slice(&(MAX_FPS + 1).to_le_bytes());
        assert!(video_config(&fast).is_err());
        assert!(video_config(&[1, 0, 0]).is_err());
    }

    #[test]
    fn ready_search_skips_an_earlier_session_and_partial_records() {
        let token = [7; 16];
        let ready = session_ready(&token);
        let mut stale = ready;
        stale[27] = 8;
        assert_eq!(ready_offset(&stale, &token), None);
        let mut stream = vec![0xff; 13];
        stream.extend_from_slice(&stale);
        let offset = stream.len();
        stream.extend_from_slice(&ready);
        assert_eq!(ready_offset(&stream, &token), Some(offset));
        assert_eq!(ready_offset(&ready[..READY_SEARCH_KEEP], &token), None);
    }

    #[test]
    fn reset_status_requires_magic_phase_and_token() {
        let mut status = [0_u8; 24];
        status[..4].copy_from_slice(b"JKV5");
        status[4] = 1;
        status[8..].fill(7);
        assert_eq!(reset_status(&status).unwrap(), (1, [7; 16]));
        assert!(reset_status(&status[..23]).is_err());
        status[4] = 9;
        assert!(reset_status(&status).is_err());
        status[4] = 1;
        status[8..].fill(0);
        assert!(reset_status(&status).is_err());
    }
}
