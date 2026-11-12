//! The USB link to the KVM: finding its OS Services interface, the vendor
//! control reset that starts a session, the hello, and the two bulk
//! endpoints, each owned by one thread for the life of the session.

use super::codec::{self, Message};
use super::*;
use nusb::{
    descriptors::TransferType,
    transfer::{Buffer, Bulk, ControlIn, ControlType, In, Out, Recipient},
    Endpoint, Interface, MaybeFuture,
};
use std::{
    collections::HashSet,
    error::Error,
    io,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::Instant,
};

/// The interface the KVM exposes for OS Services.
const INTERFACE_CLASS: u8 = 0xff;
const INTERFACE_SUBCLASS: u8 = 0x4a;
const INTERFACE_PROTOCOL: u8 = 0x01;
/// The product string of a JetKVM; a device that reports it is tried first.
const JETKVM_PRODUCT: &str = "JetKVM USB Emulation Device";

/// Vendor control requests on the interface. RESET cancels the previous
/// session on the KVM and returns a new token; STATUS reports the reset's
/// progress.
const REQUEST_RESET: u8 = 0x40;
const REQUEST_STATUS: u8 = 0x41;
const RESET_STATUS_LEN: u16 = 24;
/// The phases that STATUS reports, as `PROTOCOL.md` names them.
const RESETTING: u8 = 0;
const READY: u8 = 1;
const ACTIVE: u8 = 2;
const FAULTED: u8 = 3;

/// The KVM-to-sender endpoint carries file bytes as well as control
/// records. Several large reads in flight keep it busy between completions;
/// one small read costs a USB round trip per packet.
const READ_BUFFER: usize = 64 << 10;
const READ_QUEUE_DEPTH: usize = 4;
/// Longest wait for one bulk write. The KVM can pause its reads for seconds
/// while it restores HDMI capture.
const TRANSFER_TIMEOUT: Duration = Duration::from_secs(30);
/// How often a waiting transfer checks for cancellation.
const CANCEL_POLL: Duration = Duration::from_millis(25);

/// An open session: the KVM has answered the hello, and no service is
/// registered yet.
pub struct Negotiated {
    /// The KVM's product name and USB id, for the log.
    pub device: String,
    /// The fingerprint of the key the KVM signed the handshake with.
    pub kvm: Fingerprint,
    pub tx: UsbTx,
    pub rx: UsbRx,
    pub settings: SessionSettings,
}

/// Finds the KVM's OS Services interface and opens a session on it, with a
/// KVM whose key is in `approved`. When every interface completes the
/// handshake and none belongs to an approved KVM, the error is
/// `NotApproved`. When the sender may open none of them, it is
/// `UsbAccessDenied`. A failed handshake can be the approved KVM's, so it is
/// reported instead, and the sender tries again.
pub fn find_and_negotiate(approved: &[Fingerprint]) -> Result<Negotiated, Box<dyn Error>> {
    let candidates = interfaces()?;
    if candidates.is_empty() {
        return Err("no OS Services USB interface found".into());
    }
    let (mut failures, mut denied, mut not_approved) = (Vec::new(), Vec::new(), None);
    for (info, interface_number) in candidates {
        let device = format!(
            "{} ({:04x}:{:04x})",
            info.product_string().unwrap_or("unknown KVM"),
            info.vendor_id(),
            info.product_id(),
        );
        match negotiate(&info, interface_number, approved) {
            Ok((kvm, tx, rx, settings)) => {
                return Ok(Negotiated {
                    device,
                    kvm,
                    tx,
                    rx,
                    settings,
                })
            }
            Err(error) if error.is::<NotApproved>() => not_approved = Some(error),
            Err(error) if is_access_denied(error.as_ref()) => denied.push(device),
            Err(error) => failures.push(format!("{device}: {error}")),
        }
    }
    Err(handshake_failure(failures, denied, not_approved))
}

/// Why no interface opened a session. Any other failure comes first: it can
/// be the approved KVM's, and the sender tries again. A KVM that the sender
/// may not open comes next, because the user can fix that from the desktop.
fn handshake_failure(
    mut failures: Vec<String>,
    denied: Vec<String>,
    not_approved: Option<Box<dyn Error>>,
) -> Box<dyn Error> {
    if failures.is_empty() && !denied.is_empty() {
        return UsbAccessDenied(denied).into();
    }
    if let Some(error) = not_approved.filter(|_| failures.is_empty()) {
        return error;
    }
    failures.extend(
        denied
            .iter()
            .map(|device| format!("{device}: {ACCESS_DENIED}")),
    );
    format!("{HANDSHAKE_FAILED}: {}", failures.join("; ")).into()
}

fn is_access_denied(error: &(dyn Error + 'static)) -> bool {
    error
        .downcast_ref::<io::Error>()
        .is_some_and(|error| error.kind() == io::ErrorKind::PermissionDenied)
}

/// The devices with the OS Services interface that are attached.
pub fn attached_interfaces() -> HashSet<nusb::DeviceId> {
    interfaces()
        .map(|found| found.iter().map(|(info, _)| info.id()).collect())
        .unwrap_or_default()
}

/// Every device with the interface, JetKVMs first.
fn interfaces() -> Result<Vec<(nusb::DeviceInfo, u8)>, Box<dyn Error>> {
    let mut found: Vec<_> = nusb::list_devices()
        .wait()?
        .filter_map(|info| {
            let number = info
                .interfaces()
                .find(|interface| {
                    (
                        interface.class(),
                        interface.subclass(),
                        interface.protocol(),
                    ) == (INTERFACE_CLASS, INTERFACE_SUBCLASS, INTERFACE_PROTOCOL)
                })?
                .interface_number();
            Some((info, number))
        })
        .collect();
    found.sort_by_key(|(info, _)| info.product_string() != Some(JETKVM_PRODUCT));
    Ok(found)
}

fn negotiate(
    info: &nusb::DeviceInfo,
    interface_number: u8,
    approved: &[Fingerprint],
) -> Result<(Fingerprint, UsbTx, UsbRx, SessionSettings), Box<dyn Error>> {
    let interface = claim(info, interface_number)?;
    // The reset needs no bulk endpoint: none is opened, and no read queued,
    // until the KVM has cancelled and reaped the previous session.
    let token = reset_session(&interface, interface_number)?;
    let (out_address, in_address) = bulk_endpoints(&interface)?;
    let cancelled = Arc::new(AtomicBool::new(false));
    let mut tx = UsbTx {
        endpoint: interface.endpoint::<Bulk, Out>(out_address)?,
        cancelled: cancelled.clone(),
    };
    let mut rx = UsbRx {
        endpoint: interface.endpoint::<Bulk, In>(in_address)?,
        buffer: Vec::new(),
        cancelled,
    };
    rx.arm();
    let mut nonce = [0; 32];
    getrandom::fill(&mut nonce)?;
    let hello = codec::hello(&token, &nonce);
    tx.send(&hello)?;
    let deadline = Instant::now() + NEGOTIATION_TIMEOUT;
    let ready = rx.wait_for_ready(&token, deadline)?;
    let kvm = identity::verify_ready(&hello.payload, &ready.payload)?;
    if !approved.contains(&kvm) {
        tx.send(&codec::close(CloseReason::NotApproved))?;
        return Err(NotApproved(kvm).into());
    }
    let config = rx.wait_for_message(deadline)?;
    if (config.service, config.kind) != (SESSION_SERVICE, SESSION_CONFIG) {
        return Err("the KVM did not send session.config after session.ready".into());
    }
    let settings = serde_json::from_slice(&config.payload)?;
    Ok((kvm, tx, rx, settings))
}

fn claim(info: &nusb::DeviceInfo, interface_number: u8) -> Result<Interface, Box<dyn Error>> {
    let device = info.open().wait().map_err(|error| {
        if error.kind() == nusb::ErrorKind::PermissionDenied {
            io::Error::new(io::ErrorKind::PermissionDenied, ACCESS_DENIED)
        } else {
            io::Error::from(error)
        }
    })?;
    Ok(device.claim_interface(interface_number).wait()?)
}

/// The OUT and IN bulk endpoint addresses of the interface.
fn bulk_endpoints(interface: &Interface) -> Result<(u8, u8), Box<dyn Error>> {
    let descriptor = interface
        .descriptor()
        .ok_or("the OS Services interface has no active descriptor")?;
    let (mut out_address, mut in_address) = (None, None);
    for endpoint in descriptor
        .endpoints()
        .filter(|endpoint| endpoint.transfer_type() == TransferType::Bulk)
    {
        if endpoint.address() & 0x80 == 0 {
            out_address = Some(endpoint.address());
        } else {
            in_address = Some(endpoint.address());
        }
    }
    Ok((
        out_address.ok_or("the OS Services interface has no bulk OUT endpoint")?,
        in_address.ok_or("the OS Services interface has no bulk IN endpoint")?,
    ))
}

/// Asks the KVM to end any previous session and waits until it has; returns
/// the new session's token.
fn reset_session(interface: &Interface, number: u8) -> Result<[u8; 16], Box<dyn Error>> {
    let deadline = Instant::now() + NEGOTIATION_TIMEOUT;
    let request = |request| -> Result<(u8, [u8; 16]), Box<dyn Error>> {
        let reply = interface
            .control_in(
                ControlIn {
                    control_type: ControlType::Vendor,
                    recipient: Recipient::Interface,
                    request,
                    value: PROTOCOL_MAJOR,
                    index: number.into(),
                    length: RESET_STATUS_LEN,
                },
                Duration::from_secs(2),
            )
            .wait()?;
        Ok(codec::reset_status(&reply)?)
    };
    let (_, token) = request(REQUEST_RESET)?;
    loop {
        if Instant::now() >= deadline {
            return Err("the KVM did not finish resetting its USB session".into());
        }
        let (phase, current) = request(REQUEST_STATUS)?;
        if current != token {
            return Err("the KVM session reset was superseded".into());
        }
        match phase {
            RESETTING => thread::sleep(CANCEL_POLL),
            READY => return Ok(token),
            ACTIVE => return Err("the KVM session is already in use".into()),
            FAULTED => {
                return Err("the KVM could not cancel its USB requests; restart the KVM".into())
            }
            other => return Err(format!("the KVM reported an unknown reset phase {other}").into()),
        }
    }
}

/// The sender-to-KVM half of a link, as the session's worker writes it:
/// the OUT endpoint, or a recorder in the tests.
pub trait Transmit: Send {
    /// The flag that cancels the session; both halves share it.
    fn cancellation(&self) -> &Arc<AtomicBool>;
    fn is_cancelled(&self) -> bool {
        self.cancellation().load(Ordering::Acquire)
    }
    fn send(&mut self, message: &Message) -> Result<(), Box<dyn Error>>;
    /// Writes one video.frame.
    fn send_frame(
        &mut self,
        unit: &[u8],
        capture_us: u64,
        keyframe: bool,
        parameter_sets: bool,
    ) -> Result<(), Box<dyn Error>>;
}

/// The sender-to-KVM half of the link.
pub struct UsbTx {
    endpoint: Endpoint<Bulk, Out>,
    cancelled: Arc<AtomicBool>,
}

impl Transmit for UsbTx {
    fn cancellation(&self) -> &Arc<AtomicBool> {
        &self.cancelled
    }

    fn send(&mut self, message: &Message) -> Result<(), Box<dyn Error>> {
        for chunk in message.encode()?.chunks(TRANSFER_CHUNK) {
            self.transfer(chunk.to_vec())?;
        }
        Ok(())
    }

    /// The header and the start of the access unit share the first
    /// transfer.
    fn send_frame(
        &mut self,
        unit: &[u8],
        capture_us: u64,
        keyframe: bool,
        parameter_sets: bool,
    ) -> Result<(), Box<dyn Error>> {
        let header = codec::frame_header(unit.len(), capture_us, keyframe, parameter_sets)?;
        let (head, tail) = unit.split_at(unit.len().min(TRANSFER_CHUNK - header.len()));
        let mut first = Vec::with_capacity(header.len() + head.len());
        first.extend_from_slice(&header);
        first.extend_from_slice(head);
        self.transfer(first)?;
        for chunk in tail.chunks(TRANSFER_CHUNK) {
            self.transfer(chunk.to_vec())?;
        }
        Ok(())
    }
}

impl UsbTx {
    fn transfer(&mut self, chunk: Vec<u8>) -> Result<(), Box<dyn Error>> {
        if self.is_cancelled() {
            return Err("USB session cancelled".into());
        }
        self.endpoint.submit(chunk.into());
        let deadline = Instant::now() + TRANSFER_TIMEOUT;
        while !self.is_cancelled() && Instant::now() < deadline {
            if let Some(completion) = self.endpoint.wait_next_complete(CANCEL_POLL) {
                completion.into_result()?;
                return Ok(());
            }
        }
        self.cancelled.store(true, Ordering::Release);
        self.endpoint.cancel_all();
        // Cancellation completes asynchronously. The endpoint stays owned
        // until its transfer is reaped, so no other session overlaps it.
        while self.endpoint.wait_next_complete(CANCEL_POLL).is_none() {}
        Err("USB session cancelled or write timed out".into())
    }
}

/// The KVM-to-sender half of the link.
pub struct UsbRx {
    endpoint: Endpoint<Bulk, In>,
    buffer: Vec<u8>,
    cancelled: Arc<AtomicBool>,
}

impl UsbRx {
    pub fn cancellation(&self) -> Arc<AtomicBool> {
        self.cancelled.clone()
    }

    /// The next control record, waiting up to `timeout` for the KVM to send
    /// one.
    pub fn next_record(
        &mut self,
        timeout: Duration,
    ) -> Result<Option<ControlRecord>, Box<dyn Error>> {
        let message = match codec::take_message(&mut self.buffer)? {
            Some(message) => Some(message),
            None => {
                self.receive(timeout)?;
                codec::take_message(&mut self.buffer)?
            }
        };
        match message {
            Some(message) => Ok(codec::decode(&message)?),
            None => Ok(None),
        }
    }

    /// Keeps the read queue full. Bulk transfers on one endpoint complete in
    /// submission order, so the reads reassemble the stream in order.
    fn arm(&mut self) {
        let size = READ_BUFFER.next_multiple_of(self.endpoint.max_packet_size().max(1));
        while self.endpoint.pending() < READ_QUEUE_DEPTH {
            self.endpoint.submit(Buffer::new(size));
        }
    }

    /// Appends one completed read to the buffer; false when none completed
    /// within `timeout`.
    fn receive(&mut self, timeout: Duration) -> Result<bool, Box<dyn Error>> {
        if self.cancelled.load(Ordering::Acquire) {
            return Err("USB session cancelled".into());
        }
        let Some(completion) = self.endpoint.wait_next_complete(timeout) else {
            return Ok(false);
        };
        self.buffer
            .extend_from_slice(completion.into_result()?.as_ref());
        self.arm();
        Ok(true)
    }

    fn receive_until(&mut self, deadline: Instant, what: &str) -> Result<(), Box<dyn Error>> {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() || !self.receive(remaining)? {
            return Err(format!("timed out waiting for the KVM's {what}").into());
        }
        Ok(())
    }

    /// Discards everything before the session.ready for `token`, then
    /// takes it.
    fn wait_for_ready(
        &mut self,
        token: &[u8; 16],
        deadline: Instant,
    ) -> Result<Message, Box<dyn Error>> {
        let mut discarded = 0;
        loop {
            if let Some(offset) = codec::ready_offset(&self.buffer, token) {
                self.buffer.drain(..offset);
                return self.wait_for_message(deadline);
            }
            let discard = self.buffer.len().saturating_sub(codec::READY_SEARCH_KEEP);
            self.buffer.drain(..discard);
            discarded += discard;
            if discarded > MAX_CONTROL_PAYLOAD + TRANSFER_CHUNK {
                return Err(
                    "the KVM's session.ready did not arrive within the search limit".into(),
                );
            }
            self.receive_until(deadline, "session.ready")?;
        }
    }

    fn wait_for_message(&mut self, deadline: Instant) -> Result<Message, Box<dyn Error>> {
        loop {
            if let Some(message) = codec::take_message(&mut self.buffer)? {
                return Ok(message);
            }
            self.receive_until(deadline, "control message")?;
        }
    }
}

impl Drop for UsbRx {
    fn drop(&mut self) {
        self.endpoint.cancel_all();
        while self.endpoint.pending() != 0 {
            let _ = self.endpoint.wait_next_complete(CANCEL_POLL);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KVM: &str = "JetKVM USB Emulation Device (399b:0001)";
    const OTHER: &str = "USB Emulation Device (1d6b:0104)";

    fn not_approved() -> Option<Box<dyn Error>> {
        let kvm = Fingerprint::parse("34:75:0F:98:BD:59:FC:FC").unwrap();
        Some(NotApproved(kvm).into())
    }

    #[test]
    fn a_kvm_the_sender_may_not_open_asks_for_usb_access() {
        let error = handshake_failure(Vec::new(), vec![KVM.into()], None);
        let denied = error.downcast_ref::<UsbAccessDenied>().unwrap();
        assert_eq!(denied.0, [KVM]);
        // The log line stays the one users and the troubleshooting guide know.
        assert_eq!(
            error.to_string(),
            format!("no OS Services USB interface completed the handshake: {KVM}: {ACCESS_DENIED}")
        );
    }

    #[test]
    fn access_comes_before_approval() {
        let error = handshake_failure(Vec::new(), vec![KVM.into()], not_approved());
        assert!(error.is::<UsbAccessDenied>());
    }

    #[test]
    fn another_failure_comes_before_access() {
        let failures = vec![format!("{OTHER}: the KVM session is already in use")];
        let error = handshake_failure(failures, vec![KVM.into()], not_approved());
        assert!(!error.is::<UsbAccessDenied>());
        assert!(!error.is::<NotApproved>());
        assert_eq!(
            error.to_string(),
            format!(
                "no OS Services USB interface completed the handshake: \
                 {OTHER}: the KVM session is already in use; {KVM}: {ACCESS_DENIED}"
            )
        );
    }

    #[test]
    fn an_unapproved_kvm_alone_asks_for_approval() {
        let error = handshake_failure(Vec::new(), Vec::new(), not_approved());
        assert!(error.is::<NotApproved>());
    }

    #[test]
    fn only_a_denied_open_counts_as_missing_access() {
        let denied: Box<dyn Error> =
            io::Error::new(io::ErrorKind::PermissionDenied, ACCESS_DENIED).into();
        let busy: Box<dyn Error> = io::Error::from(io::ErrorKind::ResourceBusy).into();
        let text: Box<dyn Error> = "permission denied".into();
        assert!(is_access_denied(denied.as_ref()));
        assert!(!is_access_denied(busy.as_ref()));
        assert!(!is_access_denied(text.as_ref()));
    }
}
