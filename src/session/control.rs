//! The IN endpoint side of a session: the reader thread that consumes the
//! KVM's records at USB speed, and the link through which the streaming
//! thread serves them, with the shells and the clipboard it answers.

use super::{worker::Worker, CONTROL_POLL};
use crate::approval::Approval;
use crate::protocol::{ControlRecord, NotApproved, ServiceReply, SessionSettings, UsbRx};
use crate::services::{clipboard, file_transfer, terminal, update};
use crate::supervisor::update::Staged;
use crate::video::display;
use std::{
    collections::VecDeque,
    error::Error,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    },
    thread,
    time::{Duration, Instant},
};

/// How long the reader blocks on the IN endpoint per wait; it checks the
/// stop flag between waits.
const READ_WAIT: Duration = Duration::from_millis(50);
/// How often the KVM is told which displays are online.
const SESSION_INFO_INTERVAL: Duration = Duration::from_secs(5);
/// The most displays one session.info names.
const MAX_REPORTED_DISPLAYS: usize = 64;

/// What ends the link's wait: a record from the KVM, the error that ended
/// the reader, or output a shell printed.
enum Input {
    Record(Result<ControlRecord, String>),
    ShellOutput,
}

/// Owns the IN endpoint for the session and reads it on its own thread, so
/// control records and file and update chunks are consumed at USB speed
/// whatever the streaming thread is doing. File and update records go
/// straight to their services; every other record queues for the link.
pub struct ControlReader {
    inputs: mpsc::Receiver<Input>,
    /// Feeds `inputs` the shells' wakes.
    wakes: mpsc::Sender<Input>,
    /// A wake is queued: one is enough until the link takes it, since the
    /// link then drains every shell's output.
    wake_queued: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    /// The user removed the KVM from the approved list: the reader ends
    /// the file and update services and drops every record it reads.
    refused: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl ControlReader {
    pub fn spawn(mut rx: UsbRx, files: file_transfer::Files, updates: update::Updates) -> Self {
        let (tx, inputs) = mpsc::channel();
        let wakes = tx.clone();
        let stop = rx.cancellation();
        let thread_stop = stop.clone();
        let refused = Arc::new(AtomicBool::new(false));
        let thread_refused = refused.clone();
        let thread = thread::Builder::new()
            .name("usb-reader".into())
            .spawn(move || {
                let mut failure = None;
                let mut services = Some((files, updates));
                while failure.is_none() && !thread_stop.load(Ordering::Relaxed) {
                    let record = rx.next_record(READ_WAIT);
                    if thread_refused.load(Ordering::Relaxed) {
                        // The services end here and discard a file or a
                        // package they received in part. The endpoint is
                        // still read, so the KVM's writes do not block
                        // while the session closes.
                        services = None;
                    }
                    match (record, &mut services) {
                        (Err(err), _) => failure = Some(err.to_string()),
                        (Ok(None), _) | (Ok(Some(_)), None) => {}
                        (Ok(Some(ControlRecord::File(record))), Some((files, _))) => {
                            files.handle(record)
                        }
                        (Ok(Some(ControlRecord::Update(record))), Some((_, updates))) => {
                            updates.handle(record)
                        }
                        (Ok(Some(record)), Some((files, _))) => {
                            // The file policy applies here, ahead of any
                            // chunk that follows the config.
                            if let ControlRecord::SessionConfig(settings) = &record {
                                files.set_allowed(settings.files_enabled);
                            }
                            if tx.send(Input::Record(Ok(record))).is_err() {
                                break;
                            }
                        }
                    }
                }
                // The shells' wakes keep the channel open, so the link
                // learns of the end from this message, not a disconnect.
                let failure = failure.unwrap_or_else(|| "the USB reader stopped".into());
                let _ = tx.send(Input::Record(Err(failure)));
                thread_stop.store(true, Ordering::Release);
            })
            .expect("spawn the USB reader");
        Self {
            inputs,
            wakes,
            wake_queued: Arc::default(),
            stop,
            refused,
            thread: Some(thread),
        }
    }

    /// Ends the link's wait from a shell's thread.
    fn shell_wake(&self) -> impl Fn() + Send + Sync + 'static {
        let wakes = self.wakes.clone();
        let queued = self.wake_queued.clone();
        move || {
            if !queued.swap(true, Ordering::AcqRel) {
                let _ = wakes.send(Input::ShellOutput);
            }
        }
    }

    fn stop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }

    /// The next queued record, waiting up to `wait` for one, or the error
    /// that ended the reader. A shell's output ends the wait without one.
    fn take(&mut self, wait: Duration) -> Result<Option<ControlRecord>, Box<dyn Error>> {
        match self.inputs.recv_timeout(wait) {
            Ok(Input::Record(Ok(record))) => Ok(Some(record)),
            Ok(Input::Record(Err(err))) => Err(err.into()),
            Ok(Input::ShellOutput) => {
                self.wake_queued.store(false, Ordering::Release);
                Ok(None)
            }
            Err(mpsc::RecvTimeoutError::Timeout) => Ok(None),
            Err(mpsc::RecvTimeoutError::Disconnected) => Err("the USB reader stopped".into()),
        }
    }
}

impl Drop for ControlReader {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The records the reader hands the streaming thread, and what the link
/// serves from them itself: the session policy, the clipboard and the
/// shells. A record for the streaming loop that arrives while the loop waits
/// on something else (a display being placed, an encoder opening) is kept
/// for it, in order.
pub struct ControlLink<'a> {
    reader: ControlReader,
    terminals: terminal::Terminals,
    files_allowed: bool,
    capture_display: Option<String>,
    /// The sender sets the mode of the KVM display and tells this KVM so.
    sets_display_mode: bool,
    deferred: VecDeque<ControlRecord>,
    /// The KVM answers the clipboard registration with its setting.
    clipboard_enabled: bool,
    next_report: Instant,
    shutdown: &'a AtomicBool,
    /// The update package that completed; the session ends to install it.
    staged: update::StagedUpdate,
    /// The user's approval of this KVM; the session ends when it is gone.
    approval: Approval,
}

impl<'a> ControlLink<'a> {
    pub fn new(
        reader: ControlReader,
        settings: &SessionSettings,
        shutdown: &'a AtomicBool,
        staged: update::StagedUpdate,
        approval: Approval,
    ) -> Self {
        let wake = reader.shell_wake();
        Self {
            reader,
            terminals: terminal::Terminals::new(settings.terminal_enabled, wake),
            files_allowed: settings.files_enabled,
            capture_display: capture_display(settings),
            sets_display_mode: crate::video::display::SETS_MODE && settings.display_mode_by_sender,
            deferred: VecDeque::new(),
            clipboard_enabled: false,
            next_report: Instant::now(),
            shutdown,
            staged,
            approval,
        }
    }

    pub fn stop_reader(&mut self) {
        self.reader.stop();
    }

    /// The process is stopping, an update is ready to install, or the user
    /// removed this KVM from the approved list.
    pub fn shutdown_requested(&self) -> bool {
        self.shutdown.load(Ordering::Relaxed)
            || self.staged.lock().expect("staged update lock").is_some()
            || self.approval_withdrawn()
    }

    /// Whether the user removed this KVM from the approved list. From then
    /// on the reader serves no record.
    pub fn approval_withdrawn(&self) -> bool {
        let withdrawn = self.approval.withdrawn();
        if withdrawn {
            self.reader.refused.store(true, Ordering::Relaxed);
        }
        withdrawn
    }

    /// Fails with `NotApproved` when the approval is withdrawn.
    fn check_approval(&self) -> Result<(), Box<dyn Error>> {
        if self.approval_withdrawn() {
            return Err(NotApproved(self.approval.kvm()).into());
        }
        Ok(())
    }

    /// Ends everything the KVM reaches through the link: the shells, and
    /// the file and update services of the reader. The reader runs on, and
    /// the USB link stays open, so the writer can send session.close.
    pub fn end_services(&mut self) {
        self.terminals.set_shell(false, &mut Vec::new());
        self.reader.refused.store(true, Ordering::Relaxed);
    }

    /// The update package the session ended for, taken once.
    pub fn take_staged(&self) -> Option<Staged> {
        self.staged.lock().expect("staged update lock").take()
    }

    /// Whether the sender sets the mode of the KVM display in this session:
    /// this build does, and the KVM's first session.config asked for it.
    pub fn sets_display_mode(&self) -> bool {
        self.sets_display_mode
    }

    /// The display the user picked in the KVM's web UI, if any.
    pub fn capture_display(&self) -> Option<String> {
        self.capture_display.clone()
    }

    pub fn terminal_enabled(&self) -> bool {
        self.terminals.shell_enabled()
    }

    pub fn files_allowed(&self) -> bool {
        self.files_allowed
    }

    /// One pass: reports the displays when due, serves the session policy,
    /// the clipboard and the shells, and keeps what is left for the
    /// streaming loop. It takes every record that is ready, waiting up to
    /// `wait` for the first, so a request is answered as soon as it arrives.
    /// A KVM that the user removed from the approved list is served nothing:
    /// the pass fails with `NotApproved`, also for a caller that waits on
    /// the encoder and does not ask whether to stop. The approval is asked
    /// before each record, because a KVM that sends records faster than
    /// the pass serves them keeps the pass from its end.
    fn pass(&mut self, worker: &Worker, mut wait: Duration) -> Result<(), Box<dyn Error>> {
        self.check_approval()?;
        if Instant::now() >= self.next_report {
            let mut displays = display::online_names();
            displays.truncate(MAX_REPORTED_DISPLAYS);
            worker.reply(ServiceReply::SessionInfo { displays });
            self.next_report = Instant::now() + SESSION_INFO_INTERVAL;
        }
        // Shell output that collects for its interval ends the wait when
        // the interval does.
        if let Some(due) = self.terminals.output_due_in() {
            wait = wait.min(due);
        }
        let mut replies = Vec::new();
        while let Some(record) = self.reader.take(wait)? {
            self.check_approval()?;
            self.handle(record, worker, &mut replies);
            wait = Duration::ZERO;
        }
        self.terminals
            .poll(worker.terminal_output_budget(), &mut replies);
        for reply in replies {
            worker.reply(ServiceReply::Terminal(reply));
        }
        Ok(())
    }

    /// Serves one record, or keeps it for the streaming loop.
    fn handle(
        &mut self,
        record: ControlRecord,
        worker: &Worker,
        replies: &mut Vec<crate::protocol::TerminalReply>,
    ) {
        match record {
            ControlRecord::SessionConfig(settings) => {
                self.apply_session_config(settings, worker, replies)
            }
            ControlRecord::ClipboardConfig { enabled } => self.clipboard_enabled = enabled,
            record if record.is_terminal() => self.terminals.handle(record, replies),
            record => match clipboard::answer(&record, self.clipboard_enabled) {
                Some(reply) => worker.reply(ServiceReply::Clipboard(reply)),
                None => self.deferred.push_back(record),
            },
        }
    }

    /// The session policy, applied each time the KVM sends it: the shells
    /// and the file service follow it at once, video follows the picked
    /// display.
    fn apply_session_config(
        &mut self,
        settings: SessionSettings,
        worker: &Worker,
        replies: &mut Vec<crate::protocol::TerminalReply>,
    ) {
        self.terminals.set_shell(settings.terminal_enabled, replies);
        if self.files_allowed != settings.files_enabled {
            self.files_allowed = settings.files_enabled;
            let capabilities = file_transfer::capabilities(settings.files_enabled);
            worker.register(crate::protocol::Registration::File(capabilities));
        }
        self.capture_display = capture_display(&settings);
    }

    /// A pass while the streaming loop waits on something else; records
    /// for the loop are kept for it.
    pub fn serve(&mut self, worker: &Worker) -> Result<(), Box<dyn Error>> {
        self.pass(worker, Duration::ZERO)
    }

    /// The next record for the streaming loop, kept records first. With
    /// none kept, a pass waits up to `wait` for the KVM's next record.
    pub fn next(
        &mut self,
        worker: &Worker,
        wait: Duration,
    ) -> Result<Option<ControlRecord>, Box<dyn Error>> {
        if self.deferred.is_empty() {
            self.pass(worker, wait)?;
        }
        Ok(self.deferred.pop_front())
    }

    /// Serves the session until `step` picks something out of a pass (Some),
    /// the session fails (Err), or a stop is requested (None). Records `step`
    /// does not take are consumed: nothing else waits for them.
    pub fn serve_until<T>(
        &mut self,
        worker: &Worker,
        mut step: impl FnMut(Option<ControlRecord>) -> Result<Option<T>, Box<dyn Error>>,
    ) -> Result<Option<T>, Box<dyn Error>> {
        loop {
            if self.shutdown_requested() {
                return Ok(None);
            }
            if let Some(failure) = worker.failure() {
                return Err(failure.into());
            }
            if let Some(found) = step(self.next(worker, CONTROL_POLL)?)? {
                return Ok(Some(found));
            }
        }
    }
}

fn capture_display(settings: &SessionSettings) -> Option<String> {
    (!settings.capture_display.is_empty()).then(|| settings.capture_display.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::approval::PinStore;
    use crate::protocol::{codec::Message, ClipboardReply, ClipboardStatus, Fingerprint, Transmit};

    /// Requests answered one after another, as the KVM's web UI sends them.
    const ROUND_TRIPS: u32 = 20;

    /// The OUT endpoint as the test sees it: each message the worker
    /// writes, as it is written.
    struct Recorder {
        written: mpsc::Sender<Message>,
        cancelled: Arc<AtomicBool>,
    }

    impl Transmit for Recorder {
        fn cancellation(&self) -> &Arc<AtomicBool> {
            &self.cancelled
        }

        fn send(&mut self, message: &Message) -> Result<(), Box<dyn Error>> {
            let _ = self.written.send(message.clone());
            Ok(())
        }

        fn send_frame(&mut self, _: &[u8], _: u64, _: bool, _: bool) -> Result<(), Box<dyn Error>> {
            Ok(())
        }
    }

    /// A list that approves one KVM, and that KVM.
    fn approved_kvm(name: &str) -> (PinStore, Fingerprint) {
        let store = PinStore::temporary(name);
        let kvm = Fingerprint::parse("34:75:0F:98:BD:59:FC:FC").unwrap();
        store.approve(&kvm).unwrap();
        (store, kvm)
    }

    fn approval(name: &str) -> Approval {
        let (store, kvm) = approved_kvm(name);
        Approval::of(store, kvm)
    }

    /// The reply the session writes to a clipboard read while sharing is
    /// off: it needs no clipboard, so only the session's own loops time it.
    fn unsupported(request_id: u32) -> Message {
        let reply = ClipboardReply::Content {
            request_id,
            status: ClipboardStatus::Unsupported,
            truncated: false,
            text: String::new(),
        };
        crate::protocol::codec::service_reply(&ServiceReply::Clipboard(reply)).unwrap()
    }

    /// The time from each request reaching the link to its reply leaving on
    /// the wire, for requests sent one after another while the session waits
    /// for a video.config that does not come, as it does while the KVM keeps
    /// video off.
    fn round_trips() -> Vec<Duration> {
        let (written, wire) = mpsc::channel();
        let worker = Worker::spawn(Recorder {
            written,
            cancelled: Arc::default(),
        });
        let (kvm, inputs) = mpsc::channel();
        let reader = ControlReader {
            inputs,
            wakes: kvm.clone(),
            wake_queued: Arc::default(),
            stop: Arc::default(),
            refused: Arc::default(),
            thread: None,
        };
        let shutdown = AtomicBool::new(false);
        let settings = SessionSettings::default();
        let mut link = ControlLink::new(
            reader,
            &settings,
            &shutdown,
            Default::default(),
            approval("round-trips"),
        );
        let receive = || wire.recv_timeout(Duration::from_secs(5)).expect("a write");
        thread::scope(|scope| {
            let session = scope.spawn(|| {
                link.serve_until(&worker, |_| Ok(None::<()>))
                    .map_err(|err| err.to_string())
            });
            // The first pass reports the displays: the loop is serving.
            receive();
            let round_trips = (1..=ROUND_TRIPS)
                .map(|request_id| {
                    let reply = unsupported(request_id);
                    let sent = Instant::now();
                    kvm.send(Input::Record(Ok(ControlRecord::ClipboardGet {
                        request_id,
                    })))
                    .unwrap();
                    while receive() != reply {}
                    sent.elapsed()
                })
                .collect();
            shutdown.store(true, Ordering::Relaxed);
            assert_eq!(session.join().unwrap(), Ok(None));
            round_trips
        })
    }

    #[test]
    fn a_request_is_answered_without_waiting_out_the_poll() {
        let mut round_trips = round_trips();
        round_trips.sort();
        let median = round_trips[round_trips.len() / 2];
        assert!(
            median < CONTROL_POLL / 2,
            "median {median:?}; round trips {round_trips:?}"
        );
    }

    /// More replies than a worker pass serves, queued at once while no frame
    /// comes: the worker yields to video between passes but does not wait
    /// for a frame while replies are left.
    #[test]
    fn queued_replies_are_written_without_waiting_for_frames() {
        let (written, wire) = mpsc::channel();
        let worker = Worker::spawn(Recorder {
            written,
            cancelled: Arc::default(),
        });
        let started = Instant::now();
        for request_id in 1..=ROUND_TRIPS {
            let reply = ClipboardReply::Content {
                request_id,
                status: ClipboardStatus::Unsupported,
                truncated: false,
                text: String::new(),
            };
            worker.reply(ServiceReply::Clipboard(reply));
        }
        for request_id in 1..=ROUND_TRIPS {
            let message = wire.recv_timeout(Duration::from_secs(5)).expect("a write");
            assert_eq!(message, unsupported(request_id));
        }
        let elapsed = started.elapsed();
        assert!(elapsed < Duration::from_millis(100), "took {elapsed:?}");
    }

    /// Output that a shell prints right after a forward collects for its
    /// interval and is written when the interval ends, although no record
    /// and no further output ends the link's wait.
    #[test]
    fn output_that_collects_is_written_when_its_interval_ends() {
        let (written, wire) = mpsc::channel();
        let worker = Worker::spawn(Recorder {
            written,
            cancelled: Arc::default(),
        });
        let (kvm, inputs) = mpsc::channel();
        let reader = ControlReader {
            inputs,
            wakes: kvm,
            wake_queued: Arc::default(),
            stop: Arc::default(),
            refused: Arc::default(),
            thread: None,
        };
        let shutdown = AtomicBool::new(false);
        let settings = SessionSettings::default();
        let mut link = ControlLink::new(
            reader,
            &settings,
            &shutdown,
            Default::default(),
            approval("output"),
        );
        let print = link.terminals.printing(7);
        let print_and_wait = |data: &[u8]| {
            let reply = crate::protocol::TerminalReply::Output {
                session_id: 7,
                data: data.to_vec(),
            };
            let message =
                crate::protocol::codec::service_reply(&ServiceReply::Terminal(reply)).unwrap();
            assert!(print(data));
            while wire.recv_timeout(Duration::from_secs(5)).expect("a write") != message {}
        };
        thread::scope(|scope| {
            let session = scope.spawn(|| {
                link.serve_until(&worker, |_| Ok(None::<()>))
                    .map_err(|err| err.to_string())
            });
            let mut waits: Vec<Duration> = (0..ROUND_TRIPS)
                .map(|_| {
                    print_and_wait(b"first");
                    let printed = Instant::now();
                    print_and_wait(b"second");
                    printed.elapsed()
                })
                .collect();
            shutdown.store(true, Ordering::Relaxed);
            assert_eq!(session.join().unwrap(), Ok(None));
            waits.sort();
            let median = waits[waits.len() / 2];
            assert!(
                median < CONTROL_POLL / 2,
                "median {median:?}; waits {waits:?}"
            );
        });
    }

    /// `--forget-kvm` runs in another process and changes only the list;
    /// the open session with that KVM ends and serves nothing more, and a
    /// session with another KVM does not end.
    #[test]
    fn a_session_ends_when_its_kvm_is_forgotten() {
        let (written, _wire) = mpsc::channel();
        let worker = Worker::spawn(Recorder {
            written,
            cancelled: Arc::default(),
        });
        let (store, kvm) = approved_kvm("forgotten");
        let other = Fingerprint::parse("00:75:0F:98:BD:59:FC:FC").unwrap();
        store.approve(&other).unwrap();
        let shutdown = AtomicBool::new(false);
        let settings = SessionSettings::default();
        let serve = |approved: Fingerprint| {
            let (kvm, inputs) = mpsc::channel();
            let reader = ControlReader {
                inputs,
                wakes: kvm,
                wake_queued: Arc::default(),
                stop: Arc::default(),
                refused: Arc::default(),
                thread: None,
            };
            let approval = Approval::of(store.clone(), approved);
            let mut link =
                ControlLink::new(reader, &settings, &shutdown, Default::default(), approval);
            let deadline = Instant::now() + Duration::from_millis(1500);
            let ended = link
                .serve_until(&worker, |_| {
                    Ok((Instant::now() >= deadline).then_some("still open"))
                })
                .map_err(|err| err.to_string());
            let refused = link
                .serve(&worker)
                .is_err_and(|err| err.is::<NotApproved>());
            (ended, refused)
        };
        assert!(store.forget(&kvm).unwrap());
        thread::scope(|scope| {
            let forgotten = scope.spawn(|| serve(kvm));
            let kept = scope.spawn(|| serve(other));
            assert_eq!(forgotten.join().unwrap(), (Ok(None), true));
            assert_eq!(kept.join().unwrap(), (Ok(Some("still open")), false));
        });
    }

    /// A session that closes at once, as for a forgotten KVM, writes
    /// session.close ahead of the replies that wait. The writer is in its
    /// first write when the close is asked for, as with a KVM that reads
    /// slowly.
    #[test]
    fn a_session_closed_at_once_writes_no_reply_that_waits() {
        struct Gated {
            written: mpsc::Sender<Message>,
            open: mpsc::Receiver<()>,
            cancelled: Arc<AtomicBool>,
        }

        impl Transmit for Gated {
            fn cancellation(&self) -> &Arc<AtomicBool> {
                &self.cancelled
            }

            /// Waits until the test drops its end of the gate.
            fn send(&mut self, message: &Message) -> Result<(), Box<dyn Error>> {
                let _ = self.open.recv();
                let _ = self.written.send(message.clone());
                Ok(())
            }

            fn send_frame(
                &mut self,
                _: &[u8],
                _: u64,
                _: bool,
                _: bool,
            ) -> Result<(), Box<dyn Error>> {
                Ok(())
            }
        }

        let (written, wire) = mpsc::channel();
        let (gate, open) = mpsc::channel();
        let mut worker = Worker::spawn(Gated {
            written,
            open,
            cancelled: Arc::default(),
        });
        for request_id in 1..=ROUND_TRIPS {
            let reply = ClipboardReply::Content {
                request_id,
                status: ClipboardStatus::Unsupported,
                truncated: false,
                text: String::new(),
            };
            worker.reply(ServiceReply::Clipboard(reply));
        }
        thread::scope(|scope| {
            let closed = scope.spawn(|| worker.stop_now());
            thread::sleep(Duration::from_millis(100));
            drop(gate);
            closed.join().unwrap();
        });
        let written: Vec<Message> = wire.try_iter().collect();
        let close = crate::protocol::codec::close(crate::protocol::CloseReason::Shutdown);
        assert_eq!(written.last(), Some(&close));
        // At most the reply that was in its write when the close was asked.
        assert!(written.len() <= 2, "wrote {} messages", written.len());
    }

    /// A KVM that sends records faster than the link serves them keeps a
    /// pass from its end. The pass refuses a forgotten KVM all the same,
    /// with records still to serve: here, more records than a pass serves
    /// between two reads of the list.
    #[test]
    fn a_pass_refuses_a_forgotten_kvm_while_records_are_left() {
        const BACKLOG: u32 = 200_000;
        const READ_INTERVAL: Duration = Duration::from_millis(5);
        // The receiver is gone, so the replies do not collect.
        let (written, _) = mpsc::channel();
        let worker = Worker::spawn(Recorder {
            written,
            cancelled: Arc::default(),
        });
        let (kvm, inputs) = mpsc::channel();
        for request_id in 1..=BACKLOG {
            let record = ControlRecord::ClipboardGet { request_id };
            kvm.send(Input::Record(Ok(record))).unwrap();
        }
        let reader = ControlReader {
            inputs,
            wakes: kvm,
            wake_queued: Arc::default(),
            stop: Arc::default(),
            refused: Arc::default(),
            thread: None,
        };
        let (store, fingerprint) = approved_kvm("backlog");
        assert!(store.forget(&fingerprint).unwrap());
        let shutdown = AtomicBool::new(false);
        let settings = SessionSettings::default();
        // The first read of the list comes after the pass has started.
        let approval = Approval::read_every(READ_INTERVAL, store, fingerprint);
        let mut link = ControlLink::new(reader, &settings, &shutdown, Default::default(), approval);
        let served = link.serve(&worker);
        assert!(served.is_err_and(|err| err.is::<NotApproved>()));
    }

    /// A shell's output ends the link's wait for the KVM's next record at
    /// once, so its output does not wait out CONTROL_POLL.
    #[test]
    fn a_shell_wake_ends_the_wait_for_a_record() {
        let (kvm, inputs) = mpsc::channel();
        let mut reader = ControlReader {
            inputs,
            wakes: kvm,
            wake_queued: Arc::default(),
            stop: Arc::default(),
            refused: Arc::default(),
            thread: None,
        };
        let wake = reader.shell_wake();
        let shell = thread::spawn(move || {
            thread::sleep(Duration::from_millis(20));
            wake();
        });
        let started = Instant::now();
        assert!(reader.take(Duration::from_secs(5)).unwrap().is_none());
        let waited = started.elapsed();
        shell.join().unwrap();
        assert!(waited < Duration::from_secs(1), "waited {waited:?}");
    }
}
