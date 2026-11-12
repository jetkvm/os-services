# IP-KVM OS Services protocol

Version 5.0. This protocol lets a program on the target computer give a KVM video, clipboard, terminal, file transfer and updates, over the KVM's USB cable.

A session works in four steps:

1. The sender resets the KVM's USB interface. The reset ends any earlier session and gives the new session a random token.
2. The sender and the KVM do a handshake. The KVM signs it, so the sender knows which KVM it talks to.
3. The sender registers each service that it can offer. The KVM answers with its settings.
4. Messages flow in both directions until one side stops.

## Conventions

| Term | Meaning |
|---|---|
| Target | The computer that the KVM is connected to. |
| Sender | The program on the target that implements this protocol, as the USB host. The IP-KVM OS Services program is a sender. Other documents call it "the service". |
| KVM | The IP-KVM device, as the USB device. |
| Browser | The web interface of the KVM, where the user sees and controls the target. |
| Session | One protocol session between the sender and the KVM, from RESET to its end. A terminal session is always called a terminal session. |
| Service | A part of the protocol with its own service ID, for example the video service. |
| Hardware capture | The capture of the target's video output by the KVM. |
| Service video | The `video.frame` stream from the sender, which the KVM shows. |
| KVM display | The display that the KVM presents to the target through its EDID (Extended Display Identification Data). |

"Must" and "should" have their [RFC 2119](https://www.rfc-editor.org/rfc/rfc2119) meanings. Other present-tense statements are also requirements. Names in prose, such as Ready or Failed, refer to the values in the tables.

Integers are little-endian. Text is UTF-8 without a terminator. Payload fields come in the listed order, without padding. `rest` is the remainder of the payload.

The video service uses terms from the H.264 and H.265 specifications: access unit, NAL (network abstraction layer) unit, VCL (video coding layer) NAL unit, IDR (instantaneous decoder refresh), CRA (clean random access), and the parameter sets VPS, SPS and PPS (video, sequence and picture parameter set). Annex B is the byte-stream format with start codes.

**Versions.** A version has a major and a minor number. The sender offers its version in `session.hello`. The KVM accepts only the same major number, and returns the lower of the two minor numbers in `session.ready`. A minor version adds only compatible features. Within a version, capabilities messages announce the optional features.

## Transport

### USB interface

The KVM exposes one vendor interface: class `0xff`, subclass `0x4a`, protocol `0x01`. It has one bulk OUT endpoint (sender to KVM) and one bulk IN endpoint (KVM to sender). The interface can have more endpoints of other transfer types, for example an interrupt endpoint that a KVM adds to select the address of its bulk IN endpoint. The protocol does not use them, and the sender ignores them. The KVM should report the Microsoft OS compatible ID `WINUSB` for the interface, so that Windows binds WinUSB without a driver package. The KVM selects its own product string, interface string and `DeviceInterfaceGUID`.

The sender tries every matching interface.

### Session reset

A session starts with a reset. The reset ends the KVM's current session and gives the new session a random token. It uses two vendor control requests on the interface:

| Field | Value |
|---|---|
| `bmRequestType` | `0xc1` (device to host, vendor, interface) |
| `bRequest` | `0x40` RESET or `0x41` STATUS |
| `wValue` | Protocol major version (`5`) |
| `wIndex` | Interface number |
| `wLength` | `24` |

The KVM stalls all other requests on the interface, and RESET and STATUS for a major version that it does not implement. It answers RESET and STATUS with 24 bytes:

| Offset | Size | Field | Value |
|---:|---:|---|---|
| 0 | 4 | `magic` | ASCII `JKV5` |
| 4 | 1 | `phase` | See below. |
| 5 | 3 | `reserved` | Zero. |
| 8 | 16 | `session_token` | Random token of the current session. All zeros is invalid. |

| Value | Phase | Meaning |
|---:|---|---|
| 0 | Resetting | The KVM is ending the previous session. |
| 1 | Ready | The KVM admitted the session for this token and waits for `session.hello`. |
| 2 | Active | The session for this token completed its handshake. |
| 3 | Faulted | The KVM's USB function failed. No session starts until the KVM recovers. |

1. The sender cancels every bulk transfer of its previous session.
2. The sender sends RESET. The KVM ends its current session immediately, generates a new token, and answers with phase 0 and that token.
3. The sender polls STATUS until the phase is 1. The KVM reports phase 1 only after every bulk request of the previous session has completed.
4. The sender opens the bulk endpoints and sends `session.hello`.

Every STATUS answer must carry the token from the RESET answer. A different token means that another RESET replaced this one, and the sender stops. The sender also stops at phase 2 or 3.

The sender polls for at most 15 seconds, with a timeout of 2 seconds for each control transfer.

A USB reconfiguration of the KVM, such as an unbind, a disable or an enable of the USB function, ends the session. The KVM admits a new session only after a new RESET.

### Byte stream

Each bulk endpoint carries a continuous stream of messages. Message boundaries do not follow USB transfer boundaries. The sender limits each bulk OUT transfer to 64 KiB.

The KVM allows 2 seconds for each of its writes, and ends the session when one does not complete. For this reason, the sender always keeps a read pending on the IN endpoint. The KVM can stop reading for several seconds during a change of video source (see [Handover](#handover)), so the sender allows 30 seconds for each of its writes.

After a reset, bytes from an earlier session can remain in the endpoints. Each side finds the start of the new session by its token and discards the bytes before it:

- The KVM searches at most 8 MiB of input for a complete `session.hello` carrying the token.
- The sender searches at most 128 KiB of input for a complete `session.ready` carrying the token.

### Envelope

Every message starts with an 8-byte envelope:

| Offset | Size | Field | Description |
|---:|---:|---|---|
| 0 | 2 | `service` | Service ID. |
| 2 | 2 | `kind` | Message kind within the service. Kind 0 is reserved in every service. |
| 4 | 4 | `payload_size` | Payload length, excluding the envelope. |
| 8 | `payload_size` | `payload` | Payload. |

| ID | Service |
|---:|---|
| 0 | Session |
| 1 | Video |
| 2 | Clipboard |
| 3 | Terminal |
| 4 | File |
| 5 | Update |

A `video.frame` payload is at most 8 MiB (8,388,608 bytes). All other payloads are at most 64 KiB (65,536 bytes).

In `video.capabilities`, `video.config` and an enabled `video.status`, widths and heights are 1 to 16,384. `max_fps` and `native_fps` are 1 to 240. `maximum_fps` is 0 to 240. In a disabled `video.status`, the size, rate and bitrate fields are zero.

### Errors and session end

An error in one service stops only that service on the KVM. An error in the framing or in the session service ends the session.

- A `payload_size` above its limit ends the session.
- A receiver skips a message with an unknown service or kind.
- An invalid message of the session service ends the session.
- An invalid message of another service faults only that service on the KVM. The stream stays aligned, because the KVM read the full payload. The KVM fails every open request of the faulted service, and ignores its messages until a new capabilities message registers it again.
- The sender ends the session on each invalid message from the KVM.

When a session ends, the KVM fails every open clipboard request, file transfer and update, ends every terminal session, and returns to hardware capture.

The KVM has no message to close a session. It ends a session by no longer reading. The sender then gets a write timeout or a USB error, and starts a new session with RESET.

## Session service

The session service opens the session, proves the KVM's identity, and carries the KVM's policy.

| Kind | Message | Direction | Payload |
|---:|---|---|---|
| 1 | `session.hello` | Sender to KVM | Version and token |
| 2 | `session.ready` | KVM to sender | Version, token and signature |
| 3 | `session.ping` | Sender to KVM | Empty |
| 4 | `session.close` | Sender to KVM | `reason` u8 |
| 5 | `session.config` | KVM to sender | JSON object |
| 6 | `session.info` | Sender to KVM | JSON object |

### Handshake

The sender sends `session.hello`. The KVM answers with `session.ready`, and directly after it `session.config`. The session is open when the sender has both.

Each side allows 15 seconds for the handshake. The KVM starts the clock when it admits the session, the sender when it sends `session.hello`. A second `session.hello` ends the session.

`session.hello`:

| Offset | Size | Field | Description |
|---:|---:|---|---|
| 0 | 2 | `protocol_major` | `5` |
| 2 | 2 | `protocol_minor` | `0` |
| 4 | 16 | `session_token` | Token from the reset exchange. |
| 20 | 32 | `nonce` | Random bytes, new for each hello. |

`session.ready`:

| Offset | Size | Field | Description |
|---:|---:|---|---|
| 0 | 2 | `protocol_major` | `5` |
| 2 | 2 | `protocol_minor` | Selected minor version. |
| 4 | 16 | `session_token` | Token from the reset exchange. |
| 20 | 32 | `public_key` | The KVM's Ed25519 public key. |
| 52 | 64 | `signature` | Ed25519 signature over the ASCII text `IP-KVM OS Services session.ready v5`, the `session.hello` payload, and bytes 0 to 51 of this payload, in that order. |

### KVM approval

The sender opens a session only with a KVM that its user approved. The KVM's key identifies it:

- The KVM generates its key pair when it first needs it, and keeps it until a factory reset.
- The fingerprint is the first 8 bytes of the SHA-256 digest of the public key, shown as eight pairs of uppercase hexadecimal digits with a colon between the pairs.
- The sender verifies the signature in `session.ready`. An invalid signature ends the session. The nonce and the token bind each signature to one session.
- For a key that its user did not approve, the sender sends `session.close` with reason 3 and registers no service. Then it asks its user to approve the fingerprint. Until a session opens, the KVM shows its fingerprint as waiting for approval.

### `session.config`

The KVM owns the session policy, and the sender has none of its own. The KVM sends the policy after `session.ready` and after each change.

| Member | Type | Description |
|---|---|---|
| `terminal_enabled` | boolean | The terminal service is allowed. |
| `files_enabled` | boolean | File reception is allowed. |
| `capture_display` | string | The display to capture, by a name from `session.info`. If it is empty, the sender selects the display. |
| `display_mode_by_sender` | boolean | Optional. False if it is not present. It does not change during a session. If it is true, the KVM accepts bit 2 of `codec_flags` in `video.capabilities` (see [Display mode](#display-mode)). |

`terminal_enabled`, `files_enabled` and `capture_display` are required. The sender ignores unknown members, and applies each change immediately:

- When a flag becomes false, the sender ends the activity of the service (shells or transfers) and withdraws the service.
- When a flag becomes true, the sender registers the service again.
- When `capture_display` changes, the sender captures the new display and starts a new stream.

### `session.info`

The sender reports its version and the target's displays, so that the user can pick a display in the browser.

| Member | Type | Limit |
|---|---|---|
| `version` | string | 64 bytes |
| `displays` | array of strings | 64 names of at most 256 bytes |

`displays` lists the connected display outputs of the target. The sender sends `session.info` after it registers its services, then every 5 seconds. A value above a limit ends the session.

### Liveness

The KVM ends the session when it receives no complete message for more than 15 seconds. To prevent this on a quiet session, the sender sends `session.ping` when it has written nothing for 2 seconds.

### `session.close`

| Reason | Meaning |
|---:|---|
| 0 | Shutdown |
| 1 | Reserved |
| 2 | Protocol error |
| 3 | The user has not approved the KVM |

Other values are invalid. After `session.close`, the sender sends no more messages. A new session starts with RESET.

## Service registration

Every service except the session service is off until the sender registers it with its capabilities message:

- The sender can register a service at any time after the handshake, in any order.
- A repeated capabilities message registers the service again with the new values.
- A capabilities message with all flags zero withdraws the service. For video, all 16 bytes are zero. Before it withdraws a service, the sender ends the activity of that service.
- A service that does not register is not available during the session.

The KVM answers `video.capabilities` with `video.config`, and `clipboard.capabilities` with `clipboard.config`. The terminal, file and update services get no answer. If `video.config` does not arrive in 15 seconds, the sender ends the session.

## Video service

The video service streams the target's display as H.264 or H.265. The KVM sends the state that it wants in `video.config`. The sender answers with the state that it applied in `video.status`, then sends frames.

| Kind | Message | Direction | Purpose |
|---:|---|---|---|
| 1 | `video.capabilities` | Sender to KVM | Register or withdraw video. |
| 2 | Reserved | | |
| 3 | `video.config` | KVM to sender | The complete desired state. |
| 4 | `video.status` | Sender to KVM | The complete applied state. |
| 5 | `video.frame` | Sender to KVM | One encoded access unit. |
| 6 | `video.request_keyframe` | KVM to sender | Ask for a keyframe. Empty payload. |

### `video.capabilities`

| Offset | Size | Field | Description |
|---:|---:|---|---|
| 0 | 4 | `codec_flags` | Bit 0: H.264. Always set when video registers. Bit 1: H.265. Bit 2: the sender sets the mode of the KVM display. Set bit 2 only if `display_mode_by_sender` is true in `session.config`. Other bits are zero. |
| 4 | 4 | `max_width` | Largest width. |
| 8 | 4 | `max_height` | Largest height. |
| 12 | 4 | `max_fps` | Largest frame-rate limit. |

16 zero bytes withdraw video. Before it withdraws video, the sender sends a disabled `video.status`. If the sender withdraws video while the KVM shows service video, the KVM faults the video service.

### `video.config`

| Offset | Size | Field | Description |
|---:|---:|---|---|
| 0 | 1 | `enabled` | 0 or 1. |
| 1 | 1 | `layout` | 0 Mirror, 1 Extend. |
| 2 | 1 | `codec` | 0 H.264, 1 H.265. |
| 3 | 1 | `reserved` | Zero. |
| 4 | 4 | `width` | Requested display width. |
| 8 | 4 | `height` | Requested display height. |
| 12 | 4 | `target_kbps` | Target bitrate. 0 selects the default of the encoder. |
| 16 | 4 | `maximum_fps` | Frame-rate limit, 0 to 240. 0 means no limit: the sender uses the native rate. |

- Each message is the complete desired state. The sender applies the messages in the order in which they arrive.
- `width` and `height` are valid in every message, also when `enabled` is 0.
- `layout` 1 puts the KVM display next to the target's other displays. `layout` 0 makes it a mirror of them.
- The sender captures the display at its current mode.
- The KVM requests H.265 only if the sender set bit 1 of `codec_flags`, and limits `maximum_fps` to `max_fps`. Settings that need a size above `max_width` or `max_height` fault the video service.

| Change | Sender action |
|---|---|
| `enabled` becomes 0 | Stop capture and the encoder, then send a disabled `video.status`. |
| `enabled` becomes 1 | If the sender set bit 2 of `codec_flags`, set the display mode. Capture the display. Send an enabled `video.status` with `stream_reset` 1. |
| `width`, `height` or `layout` | If the sender set bit 2 of `codec_flags`, set the display mode. Then put the display in position and capture it again. Start a new stream. |
| `codec` | Create a new encoder. Start a new stream. |
| `target_kbps` or `maximum_fps` | Apply the new bitrate and frame-rate limit. If the encoder restarts, start a new stream. |

The KVM can change `target_kbps` and `maximum_fps` during playback, for example when the network changes. If the running encoder accepts the new bitrate, the sender keeps the stream. Otherwise, it creates a new encoder and starts a new stream.

### Display mode

Bit 2 of `codec_flags` decides who sets the size of the KVM display: the sender, or the target's operating system.

The KVM asks for a size by setting `width` by `height` as the preferred mode in its EDID, then disconnecting and connecting its display (a hotplug). An operating system usually applies the preferred mode only to a display that it has not seen before. It gives a known display its previous mode.

- **Bit 2 set: the sender sets the mode.** The KVM presents the same display each time: for each KVM and size, the EDID serial number does not change. The sender sets the KVM display to `width` by `height` each time that the KVM applies its settings:
  - at the first `video.config` of a session
  - when `enabled` becomes 1
  - when `width`, `height` or `layout` changes

  The target can take some time to detect the new display, so the sender keeps trying for 10 seconds. It sets the mode only on the display whose preferred mode has this size, and only if that display has a different size. It sets no mode in these cases:
  - `layout` is 0 and the target has a second display. The KVM display then gets the size of the display that it mirrors.
  - More than one KVM display prefers this size.

  A mode that the user selects after that stays until the KVM applies its settings again.
- **Bit 2 clear: the operating system sets the mode.** The KVM presents a new display each time: the EDID serial number changes with each hotplug. The target uses the preferred mode, and the sender sets no mode. A mode that the user selects stays until the next hotplug.

A KVM that does not set `display_mode_by_sender` treats bit 2 as an error, as it does for all unknown bits.

### `video.status`

| Offset | Size | Field | Description |
|---:|---:|---|---|
| 0 | 1 | `enabled` | 0 or 1. |
| 1 | 1 | `layout` | Applied layout, 0 or 1. |
| 2 | 1 | `codec` | Applied codec, 0 or 1. |
| 3 | 1 | `stream_reset` | 1: a new coded sequence starts with the next frame. |
| 4 | 4 | `actual_width` | Captured width. |
| 8 | 4 | `actual_height` | Captured height. |
| 12 | 4 | `native_fps` | Access-unit rate without a limit. |
| 16 | 4 | `default_kbps` | The default bitrate of the encoder. |
| 20 | 4 | `target_kbps` | Applied target bitrate. |
| 24 | 4 | `maximum_fps` | Applied frame-rate limit, 0 to 240. 0 means no limit. |
| 28 | 4 | `error_code` | 0: no error. 1: the selected display is connected but not active, for example asleep. Use 1 only with `enabled` 0. Other values are reserved. |
| 32 | rest | `error_message` | UTF-8 text. |

The output rate is `native_fps`, or `maximum_fps` when that is not zero and is lower. In a disabled status, the size, rate and bitrate fields are zero.

The sender sends `video.status`:

- after each `video.config` that changes the applied state
- after each `video.config` with `enabled` 0, when capture has stopped
- after a display change on the target, and when it creates a new encoder or a new capture
- when capture or encoding fails

The KVM ignores a status while video is not registered. It faults the video service on an enabled status that:

- has a width, height or `native_fps` that is zero or out of range
- reports H.265, but the sender did not set bit 1 of `codec_flags`
- is larger than `max_width`, `max_height` or `max_fps`
- arrives when the KVM has not started a switch to service video

### `video.frame`

| Offset | Size | Field | Description |
|---:|---:|---|---|
| 0 | 8 | `capture_timestamp_us` | Capture time in microseconds on the sender's monotonic clock. |
| 8 | 4 | `flags` | Bit 0: keyframe. Bit 1: parameter sets present. Other bits zero. |
| 12 | rest | `access_unit` | One complete access unit in Annex B format. It must not be empty. |

- The access unit uses the codec of the last enabled `video.status`.
- Only the difference between two consecutive timestamps has a meaning. The KVM uses it as the frame duration.
- The keyframe flag must agree with the first VCL NAL unit: H.264 type 5 (IDR), or H.265 types 19 to 21 (IDR_W_RADL, IDR_N_LP, CRA).
- Each keyframe should contain the parameter sets, so that the KVM can start the video for a new browser at any keyframe.
- The KVM discards frames that arrive before the first enabled status, and frames that arrive while video is disabled or not registered.

A frame that breaks these rules faults the video service.

### Stream boundaries and keyframes

A status starts a stream, and the frames after it belong to that stream. Both travel on the same OUT endpoint, in order. After a status with `stream_reset` 1:

1. The KVM discards queued frames of the previous sequence.
2. The next frame has both flag bits set.
3. That frame contains SPS and PPS (H.264), or VPS, SPS and PPS (H.265).

If the first frame breaks these rules, the KVM faults the video service.

The KVM sends `video.request_keyframe` when a browser connects and when frames are lost. Until a keyframe arrives, it sends no delta frames to the browser. It sends the request at most every 600 milliseconds. While the IN endpoint is busy, it can skip a request, and sends it again after 600 milliseconds. The sender answers with a keyframe. There is no other acknowledgment.

### Handover

The KVM shows the target's display from one source at a time: hardware capture or service video. The browser stays connected when the source changes.

| State | Meaning |
|---|---|
| Hardware capture | The KVM shows hardware capture. |
| Switching to service video | The KVM stopped hardware capture, presented its display to the target, and waits for an enabled `video.status`. |
| Service video | The KVM shows `video.frame` data. |
| Switching to hardware capture | The KVM removed its display from the target and restarts hardware capture. |

#### Switch to service video

The KVM starts a switch when video registers and when the user turns on video, if all these conditions are true:

- Video is on in the KVM settings.
- The settings agree with the capabilities in `video.capabilities`.
- The sender set the `codec_flags` bit of the codec that the browser needs.
- The switch limit allows the switch.

The KVM stops hardware capture, presents the KVM display, and sends `video.config` with `enabled` 1. The switch completes at the first valid enabled `video.status`. If none arrives in 45 seconds, the KVM returns to hardware capture.

The switch limit protects the target from a loop of display hotplugs. It is three switches in two minutes. The KVM refuses a fourth switch in that time and blocks automatic switches. While switches are blocked, the KVM answers a registration with `enabled` 0. The block stays, also in later sessions, until the user turns video off and on in the KVM settings.

These events remove the current switch from the count, but do not remove the block:

- a disabled status with `error_code` 1
- a session that ends because the KVM restarted its USB device, for example to mount virtual media or to apply a USB setting

These events still count: a bus reset or a reconfiguration by the host, and a USB restart after the Faulted phase.

#### Switch to hardware capture

The KVM returns to hardware capture when:

- it sent `video.config` with `enabled` 0, and it receives the disabled `video.status` or 15 seconds pass without it
- the sender sends a disabled `video.status` without a request
- the KVM faults the video service
- the session ends
- a switch to service video gets no enabled status in 45 seconds

The KVM removes its display from the target immediately after the disabled status. For this reason, the sender stops capture before it sends the disabled status.

When capture or encoding cannot continue, the sender sends a disabled `video.status` and withdraws video. It registers video again when capture is possible again. When the selected display is connected but not active, the sender uses `error_code` 1, and registers video again only when that display is active. In both cases, the other services continue.

When the KVM faults the video service, it removes the video registration, sends `video.config` with `enabled` 0, and returns to hardware capture. The session, the other services and the KVM settings do not change. A new `video.capabilities` removes the fault.

## Clipboard service

The clipboard service copies plain text between the browser and the target. The KVM asks, and the sender answers: the sender never sends clipboard text without a request. The KVM keeps no copy.

| Kind | Message | Direction | Payload |
|---:|---|---|---|
| 1 | `clipboard.capabilities` | Sender to KVM | See below. |
| 2 | `clipboard.get` | KVM to sender | `request_id` u32 |
| 3 | `clipboard.content` | Sender to KVM | See below. |
| 4 | `clipboard.set` | KVM to sender | `request_id` u32, `text` rest |
| 5 | `clipboard.status` | Sender to KVM | `request_id` u32, `status` u8 |
| 6 | Reserved | | |
| 7 | `clipboard.config` | KVM to sender | `enabled` u8, 0 or 1 |

`clipboard.capabilities`:

| Offset | Size | Field | Description |
|---:|---:|---|---|
| 0 | 4 | `flags` | Bit 0: read (`clipboard.get`). Bit 1: write (`clipboard.set`). Other bits zero. |
| 4 | 1 | `os` | Target OS: 0 Unknown, 1 Windows, 2 macOS, 3 Linux. |
| 5 | 3 | `reserved` | Zero. |

`clipboard.content`:

| Offset | Size | Field | Description |
|---:|---:|---|---|
| 0 | 4 | `request_id` | ID of the `clipboard.get`. |
| 4 | 1 | `status` | See below. |
| 5 | 1 | `truncated` | 1 when the sender cut the text, else 0. |
| 6 | 2 | `reserved` | Zero. |
| 8 | rest | `text` | Empty unless `status` is OK. |

| Value | Status | Meaning |
|---:|---|---|
| 0 | OK | The request succeeded. |
| 1 | Unsupported | The sender cannot read the clipboard (for `clipboard.get`) or write it (for `clipboard.set`), or clipboard sharing is off. |
| 2 | Empty | The clipboard of the target contains no text. |
| 3 | Denied | The clipboard content is marked as secret. |
| 4 | Failed | The clipboard call failed or did not complete in time. |

- The sender answers `clipboard.get` with `clipboard.content`, and `clipboard.set` with `clipboard.status`.
- The sender sets the read and write bits only if it can open the target's clipboard. With neither bit set, the message withdraws the service, and the KVM fails every open request.
- The KVM uses `os` to select the paste shortcut that it types after `clipboard.set`.
- Sharing is off until the sender receives `clipboard.config`. The KVM sends it after each `clipboard.capabilities` and after each change of the sharing setting. While sharing is off, the KVM sends no requests, and the sender answers each request with Unsupported.
- A `request_id` identifies one open request. The sender answers each request exactly one time, in any order. After 2 seconds, the KVM stops waiting and ignores a late answer.
- Text is at most 65,528 bytes. The sender cuts longer text at a character boundary and sets `truncated`.
- The sender answers Denied for content that a password manager marks as secret: `org.nspasteboard.ConcealedType` on macOS, `ExcludeClipboardContentFromMonitorProcessing` on Windows, and `x-kde-passwordManagerHint` on Linux (X11 and Wayland).

## Terminal service

The terminal service runs shells on the target for the terminal in the browser. The KVM opens and closes each terminal session and sends the keystrokes. The sender sends the shell's output. A shell runs as the user of the desktop session in which the sender runs.

| Kind | Message | Direction | Payload |
|---:|---|---|---|
| 1 | `terminal.capabilities` | Sender to KVM | `flags` u32. Bit 0: shell. Other bits zero. |
| 2 | `terminal.open` | KVM to sender | `session_id` u32, `rows` u16, `cols` u16 |
| 3 | `terminal.opened` | Sender to KVM | `session_id` u32, `status` u8 |
| 4 | `terminal.input` | KVM to sender | `session_id` u32, `data` rest |
| 5 | `terminal.output` | Sender to KVM | `session_id` u32, `data` rest |
| 6 | `terminal.resize` | KVM to sender | `session_id` u32, `rows` u16, `cols` u16 |
| 7 | `terminal.close` | KVM to sender | `session_id` u32 |
| 8 | `terminal.exited` | Sender to KVM | `session_id` u32, `reason` u8 |
| 9 | `terminal.credit` | KVM to sender | `session_id` u32, `bytes` u32 |

| Value | `terminal.opened` status | Meaning |
|---:|---|---|
| 0 | OK | The shell runs. |
| 1 | Unsupported | The sender has no shell. |
| 2 | Busy | The sender runs its maximum number of terminal sessions. |
| 3 | Failed | The shell did not start, or the `session_id` is in use. |

| Value | `terminal.exited` reason | Meaning |
|---:|---|---|
| 0 | Exited | The shell ended. |
| 1 | Closed | The sender ended the shell after `terminal.close` or a withdrawal. |
| 2 | Failed | The sender lost the shell or could not deliver input to it. |

- The KVM gives each terminal session a non-zero `session_id`, and never uses it again in the same protocol session.
- `data` is 1 to 65,532 raw bytes for an `xterm-256color` terminal.
- In `terminal.open`, zero rows or zero columns select 24 by 80. In `terminal.resize`, both values are not zero. The KVM opens a terminal session at 0 by 0, then sends `terminal.resize`.
- The sender runs at most four terminal sessions at the same time.
- Credit keeps terminal output from delaying video frames on the OUT endpoint. The `terminal.output` of a terminal session can be up to 131,072 bytes (its window) ahead of the credit that the KVM returned for it. `terminal.credit` returns the bytes that the browser used, in steps of at least 16,384 bytes. While the browser is slow, the KVM returns no credit.
- Only an OK answer creates a terminal session. If `terminal.opened` does not arrive in 5 seconds, the KVM sends `terminal.close`.
- On `terminal.close`, the sender ends the shell and sends `terminal.exited` with reason Closed. Before it withdraws the shell capability, it ends every terminal session in this way.
- `terminal.exited` is the last message of a terminal session.
- Both sides ignore messages for an unknown `session_id`, because a terminal session can end while a message is on its way.

## File service

The file service sends files from the browser to the target. The sender writes them to the user's Downloads folder, or to the home folder if there is no Downloads folder. The KVM forwards the bytes and keeps no copy.

| Kind | Message | Direction | Payload |
|---:|---|---|---|
| 1 | `file.capabilities` | Sender to KVM | `flags` u32. Bit 0: receive. Other bits zero. |
| 2 | `file.drop` | KVM to sender | See below. |
| 3 | `file.ready` | Sender to KVM | `transfer_id` u32, `credit` u32 |
| 4 | `file.chunk` | KVM to sender | `transfer_id` u32, `offset` u64, `data` rest |
| 5 | `file.credit` | Sender to KVM | `transfer_id` u32, `bytes` u32 |
| 6 | `file.done` | Sender to KVM | See below. |
| 7 | `file.cancel` | KVM to sender | `transfer_id` u32 |

`file.drop` is `file_count` (u32, at least 1) followed by exactly that many entries:

| Size | Field | Description |
|---:|---|---|
| 4 | `transfer_id` | Non-zero. |
| 8 | `size` | File size in bytes. |
| 2 | `name_length` | Length of `name`. |
| `name_length` | `name` | One path component: not empty, not `.` or `..`, no `/`, `\` or control character. |

`file.done`:

| Offset | Size | Field | Description |
|---:|---:|---|---|
| 0 | 4 | `transfer_id` | Transfer ID. |
| 4 | 1 | `status` | See below. |
| 5 | 3 | `reserved` | Zero. |
| 8 | rest | `path` | The final path if `status` is OK, else empty. The sender cuts a longer path to 65,528 bytes at a character boundary. |

| Value | Status | Meaning |
|---:|---|---|
| 0 | OK | The sender wrote every byte. |
| 1 | Failed | The sender could not create or write the file, or file reception is off. |
| 2 | Cancelled | The KVM cancelled the transfer, or file reception was turned off. |

- Capabilities without the receive bit withdraw the service, and the KVM fails every open transfer.
- The KVM gives each transfer an ID that is non-zero and unique in the session.
- The sender writes the files of a drop one at a time, in order. If a file with the name already exists, it selects a name that is not in use.
- The sender ends a transfer with Failed, and writes nothing, if the target would not store the name as a file with exactly that name in the folder. Examples are a name with a bidirectional embedding, override or isolate character, and on Windows a name with `:`, a name that ends with `.` or a space, and a device name such as `NUL`.
- The sender sends `file.ready` one time for each transfer, after it creates the file. For a file of size 0, `file.done` follows immediately.
- Credit is cumulative. It starts at the value in `file.ready`, and each `file.credit` adds to it. The sender offers 512 KiB, and returns credit as it writes.
- A chunk has at most 65,524 data bytes and a non-zero `transfer_id`. A chunk that breaks either rule is invalid, and the sender ends the session.
- The `offset` of a chunk equals the number of bytes already sent in the transfer. The end of the chunk is not past the credit or the file size. A chunk that breaks these rules fails the transfer.
- The sender ignores chunks for a transfer that has ended.
- Every transfer of a `file.drop` ends with exactly one `file.done`. The sender deletes a partial file.
- The KVM cancels a transfer that the browser stops. If neither side makes progress for 30 seconds, the KVM cancels every open transfer of the drop.
- If a `file.drop` arrives while file reception is off, the sender ends each of its transfers with Failed.

## Update service

The update service lets the KVM replace the sender with a new release, so the target needs no network access. The KVM selects the release, gets its package and verifies the package signature. Then it sends the bytes with the size and the SHA-256 hash of the package. The sender checks the size and the hash, ends the session, installs the package and starts again.

| Kind | Message | Direction | Payload |
|---:|---|---|---|
| 1 | `update.capabilities` | Sender to KVM | `flags` u32, `package` rest |
| 2 | `update.begin` | KVM to sender | See below. |
| 3 | `update.ready` | Sender to KVM | `update_id` u32, `credit` u32 |
| 4 | `update.chunk` | KVM to sender | `update_id` u32, `offset` u64, `data` rest |
| 5 | `update.credit` | Sender to KVM | `update_id` u32, `bytes` u32 |
| 6 | `update.status` | Sender to KVM | See below. |
| 7 | `update.cancel` | KVM to sender | `update_id` u32 |

In `update.capabilities`, bit 0 of `flags` is install, and the other bits are zero. `package` names the package that the sender can install:

| `package` | Package |
|---|---|
| `windows-x86_64` | The Windows installer |
| `macos-aarch64` | The macOS app bundle as a gzip-compressed tar archive |
| `linux-x86_64-appimage` | The Linux AppImage |

`update.begin`:

| Offset | Size | Field | Description |
|---:|---:|---|---|
| 0 | 4 | `update_id` | Non-zero. |
| 4 | 8 | `size` | Package size in bytes. Non-zero. |
| 12 | 32 | `sha256` | SHA-256 of the package. |
| 44 | rest | `version` | Release version, at most 64 bytes. |

`update.status`:

| Offset | Size | Field | Description |
|---:|---:|---|---|
| 0 | 4 | `update_id` | Update ID. |
| 4 | 1 | `status` | See below. |
| 5 | 3 | `reserved` | Zero. |
| 8 | rest | `message` | Why the update failed, else empty. The sender cuts a longer message to 65,528 bytes at a character boundary. |

| Value | Status | Meaning |
|---:|---|---|
| 0 | Installing | The package is complete and its hash is correct. The sender ends the session and installs the package. |
| 1 | Failed | The sender could not write the package to its temporary file, the size or the hash is wrong, or the sender refused the package. The message gives the reason. |
| 2 | Cancelled | The KVM cancelled the update, or a new `update.begin` replaced it. |

- A sender that cannot replace itself, such as a development build, does not register the service.
- The KVM gives each update an ID that is non-zero and unique in the session.
- Only one update is open at a time. A new `update.begin` cancels the open update.
- The sender writes the package to a temporary file, and sends `update.ready` after it creates this file.
- Credit and chunks follow the rules of the file service. An invalid chunk ends the session. A chunk that breaks the offset, credit or size rules fails the update.
- Every update ends with exactly one `update.status`. If an update does not end with Installing, the sender deletes the temporary file.
- After Installing, the sender closes the session with reason 0 and sends no more messages. The new sender opens a new session, and its `session.info` reports the new version.

## Message flows

In these flows, `→ KVM` is a message from the sender, and `→ sender` is a message from the KVM.

Session start with video on:

```text
→ KVM     RESET, STATUS ... phase=1      (vendor control requests)
→ KVM     session.hello
→ sender  session.ready
→ sender  session.config
→ KVM     clipboard.capabilities, terminal.capabilities, file.capabilities,
          update.capabilities
→ KVM     video.capabilities             # if the sender can capture
→ KVM     session.info                   # then every 5 seconds
→ sender  clipboard.config
→ sender  video.config  { enabled=1 }
→ KVM     video.status  { enabled=1, stream_reset=1 }
→ KVM     video.frame   { keyframe=1, parameter_sets=1 }
→ KVM     video.frame ...
```

Video turned off and on in the KVM settings:

```text
→ sender  video.config  { enabled=0 }
→ KVM     video.status  { enabled=0 }    # the KVM returns to hardware capture
→ sender  video.config  { enabled=1 }
→ KVM     video.status  { enabled=1, stream_reset=1 }
→ KVM     video.frame   { keyframe=1, parameter_sets=1 }
```
