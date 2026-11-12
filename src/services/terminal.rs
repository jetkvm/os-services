//! Shells for the KVM's terminal panel. The KVM opens a session, sends
//! keystrokes and closes it; the shell's output streams back. Each shell runs
//! as the desktop user, and the KVM's session policy turns the service on
//! and off.

use crate::protocol::{
    ControlRecord, TerminalExitReason, TerminalOpenStatus, TerminalReply, TerminalSize,
    TERMINAL_OUTPUT_CHUNK, TERMINAL_OUTPUT_WINDOW,
};
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use std::{
    collections::HashMap,
    io::{Read, Write},
    sync::{mpsc, Arc, Condvar, Mutex},
    thread,
    time::{Duration, Instant},
};

const MAX_SESSIONS: usize = 4;
const DEFAULT_SIZE: TerminalSize = TerminalSize { rows: 24, cols: 80 };
/// One read fills at most one terminal.output, and one that fits a single
/// USB transfer.
const READ_CHUNK: usize = TERMINAL_OUTPUT_CHUNK;
/// A shell that prints faster than USB drains blocks on its own write once
/// this much output is queued, instead of growing the sender's memory. The
/// limit counts bytes, not reads: a program that flushes each line gives a
/// read per line, and the session's loop takes the queue once per frame
/// while it streams video.
const OUTPUT_QUEUE: usize = 256 << 10;
/// The least time between two forwards of output. What the shells print
/// within it goes out together, so a program that prints line by line costs
/// a few hundred USB transfers per second and not one per line. Output
/// after a pause, such as the echo of a key, is forwarded at once.
const OUTPUT_INTERVAL: Duration = Duration::from_millis(2);
/// Keystrokes waiting for a shell whose foreground program is not reading.
const INPUT_QUEUE: usize = 256;
/// Time a closed shell gets to act on its hangup before it is killed.
const HANGUP_GRACE: Duration = Duration::from_millis(500);
/// How often the wait thread looks at the shell.
const WAIT_POLL: Duration = Duration::from_millis(50);
/// Time an exited shell's last output gets to arrive before the session ends
/// without waiting for the pty to close.
const EXIT_DRAIN: Duration = Duration::from_millis(500);

enum Event {
    Output {
        session_id: u32,
        data: Vec<u8>,
    },
    /// The reader reached the end of the pty: nothing more will be printed.
    Eof {
        session_id: u32,
    },
    ChildExited {
        session_id: u32,
    },
}

struct Session {
    /// Dropping the master closes the pty, which is what brings the reader
    /// thread to EOF on Windows. Always `Some` until the session drops.
    master: Option<Box<dyn MasterPty + Send>>,
    /// Keystrokes go through a thread: a pty write blocks while the shell's
    /// input queue is full, and the capture loop that delivers them must not.
    input: mpsc::SyncSender<Vec<u8>>,
    /// Dropped with the session, which tells the wait thread to end the
    /// shell.
    _closed: mpsc::Sender<()>,
}

impl Drop for Session {
    /// Closes the pty on a thread of its own: before Windows 11 24H2,
    /// ClosePseudoConsole can block while the shell still runs, and the
    /// session's loop must not.
    fn drop(&mut self) {
        let Some(master) = self.master.take() else {
            return;
        };
        let _ = thread::Builder::new()
            .name("terminal-close".into())
            .spawn(move || drop(master));
    }
}

enum Shell {
    Running(Session),
    /// The shell has exited, and its last output is still on the way. The
    /// session ends at the reader's EOF, or at `until` for a pty that a job
    /// left behind keeps open.
    Exited {
        until: Instant,
    },
}

/// The output in the queue that `poll` has not taken, in bytes.
struct Backlog {
    /// None once the queue's reader is gone.
    bytes: Mutex<Option<usize>>,
    taken: Condvar,
}

impl Backlog {
    fn new() -> Self {
        Self {
            bytes: Mutex::new(Some(0)),
            taken: Condvar::new(),
        }
    }

    /// Counts `len` more bytes, first waiting until they fit; an empty queue
    /// takes any read. False when the queue's reader is gone.
    fn add(&self, len: usize) -> bool {
        let mut bytes = self.bytes.lock().expect("backlog lock");
        loop {
            match bytes.as_mut() {
                None => return false,
                Some(queued) if *queued == 0 || *queued + len <= OUTPUT_QUEUE => {
                    *queued += len;
                    return true;
                }
                Some(_) => bytes = self.taken.wait(bytes).expect("backlog lock"),
            }
        }
    }

    fn take(&self, len: usize) {
        if let Some(queued) = self.bytes.lock().expect("backlog lock").as_mut() {
            *queued -= len;
        }
        self.taken.notify_all();
    }

    fn close(&self) {
        *self.bytes.lock().expect("backlog lock") = None;
        self.taken.notify_all();
    }
}

/// Where a shell's threads report: the queue `poll` drains, and the wake
/// that tells the session's loop to drain it now.
#[derive(Clone)]
struct EventSink {
    events: mpsc::Sender<Event>,
    backlog: Arc<Backlog>,
    wake: Arc<dyn Fn() + Send + Sync>,
}

impl EventSink {
    fn send(&self, event: Event) -> bool {
        if let Event::Output { data, .. } = &event {
            if !self.backlog.add(data.len()) {
                return false;
            }
        }
        let sent = self.events.send(event).is_ok();
        (self.wake)();
        sent
    }
}

pub struct Terminals {
    shell: bool,
    sessions: HashMap<u32, Shell>,
    /// Output each session may still send before the KVM returns credit.
    credit: HashMap<u32, usize>,
    /// Output past its session's credit, which every later event waits
    /// behind so the order holds.
    held: Option<(u32, Vec<u8>)>,
    /// When the next output may be forwarded.
    output_due: Instant,
    /// A poll may have left output in the queue: it came before
    /// `output_due`, or its budget ended it.
    output_waits: bool,
    events: mpsc::Receiver<Event>,
    event_sink: EventSink,
}

impl Terminals {
    /// `wake` runs on a shell's thread each time it queues an event.
    pub fn new(shell: bool, wake: impl Fn() + Send + Sync + 'static) -> Self {
        let (events_tx, events) = mpsc::channel();
        Self {
            shell,
            sessions: HashMap::new(),
            credit: HashMap::new(),
            held: None,
            output_due: Instant::now(),
            output_waits: false,
            events,
            event_sink: EventSink {
                events: events_tx,
                backlog: Arc::new(Backlog::new()),
                wake: Arc::new(wake),
            },
        }
    }

    pub fn shell_enabled(&self) -> bool {
        self.shell
    }

    /// Advertises or withdraws the shell. Withdrawing ends every session
    /// first, so the KVM never sees a session outlive the capability.
    pub fn set_shell(&mut self, shell: bool, replies: &mut Vec<TerminalReply>) {
        if self.shell == shell {
            return;
        }
        self.shell = shell;
        if !shell {
            for session_id in self.sessions.keys().copied().collect::<Vec<_>>() {
                self.end(session_id, TerminalExitReason::Closed, replies);
            }
        }
        replies.push(TerminalReply::Capabilities { shell });
    }

    /// Serves a terminal control record; other records are ignored.
    pub fn handle(&mut self, record: ControlRecord, replies: &mut Vec<TerminalReply>) {
        match record {
            ControlRecord::TerminalOpen { session_id, size } => {
                let status = self.open(session_id, size);
                replies.push(TerminalReply::Opened { session_id, status });
            }
            ControlRecord::TerminalInput { session_id, data } => {
                let Some(Shell::Running(session)) = self.sessions.get(&session_id) else {
                    return;
                };
                if session.input.try_send(data).is_err() {
                    eprintln!("ipkvm-os-services: terminal {session_id} stopped taking input");
                    self.end(session_id, TerminalExitReason::Failed, replies);
                }
            }
            ControlRecord::TerminalResize { session_id, size } => {
                if let Some(Shell::Running(Session {
                    master: Some(master),
                    ..
                })) = self.sessions.get(&session_id)
                {
                    let _ = master.resize(pty_size(size));
                }
            }
            ControlRecord::TerminalClose { session_id } => {
                self.end(session_id, TerminalExitReason::Closed, replies);
            }
            ControlRecord::TerminalCredit { session_id, bytes } => {
                if let Some(credit) = self.credit.get_mut(&session_id) {
                    *credit += bytes as usize;
                }
            }
            _ => {}
        }
    }

    /// Forwards up to `budget` bytes of what the shells printed since the
    /// last call and reports the sessions that ended. What does not fit the
    /// budget or its session's credit stays queued, which is how a shell that
    /// prints faster than the browser takes it ends up blocking on its own
    /// write, and never holds up the video behind it on the wire.
    /// Consecutive output of one session is merged, so a burst costs one
    /// message per wire-sized chunk rather than one per pty read. For
    /// OUTPUT_INTERVAL after a forward the queue is left to collect, and
    /// `output_due_in` tells the caller when to poll again.
    pub fn poll(&mut self, budget: usize, replies: &mut Vec<TerminalReply>) {
        self.poll_at(Instant::now(), budget, replies);
    }

    /// How long the caller may wait before the next poll: set while a poll
    /// has left the queue to collect, or has stopped at its budget.
    pub fn output_due_in(&self) -> Option<Duration> {
        self.output_waits
            .then(|| self.output_due.saturating_duration_since(Instant::now()))
    }

    fn poll_at(&mut self, now: Instant, budget: usize, replies: &mut Vec<TerminalReply>) {
        self.end_drained(replies);
        self.output_waits = now < self.output_due;
        if self.output_waits {
            return;
        }
        let mut forwarded = 0;
        while forwarded < budget {
            let Some(event) = self.next_event() else {
                break;
            };
            match event {
                Event::Output { session_id, data } => {
                    let Some(data) = self.take_credit(session_id, data) else {
                        continue;
                    };
                    if data.is_empty() {
                        break;
                    }
                    forwarded += data.len();
                    match replies.last_mut() {
                        Some(TerminalReply::Output {
                            session_id: last,
                            data: held,
                        }) if *last == session_id
                            && held.len() + data.len() <= TERMINAL_OUTPUT_CHUNK =>
                        {
                            held.extend_from_slice(&data)
                        }
                        _ => replies.push(TerminalReply::Output { session_id, data }),
                    }
                }
                Event::Eof { session_id } => {
                    self.end(session_id, TerminalExitReason::Exited, replies);
                }
                Event::ChildExited { session_id } => {
                    if let Some(shell) = self.sessions.get_mut(&session_id) {
                        *shell = Shell::Exited {
                            until: Instant::now() + EXIT_DRAIN,
                        };
                    }
                }
            }
        }
        if forwarded > 0 {
            self.output_due = now + OUTPUT_INTERVAL;
            // The budget may have ended the loop with output still queued,
            // and no wake follows for it.
            self.output_waits = forwarded >= budget;
        }
    }

    /// Ends the exited shells whose pty a left-behind job keeps open. While
    /// output waits for credit, the queue behind it may hold any shell's
    /// last output, so none is ended.
    fn end_drained(&mut self, replies: &mut Vec<TerminalReply>) {
        if self.held.is_some() {
            return;
        }
        let now = Instant::now();
        let drained: Vec<u32> = self
            .sessions
            .iter()
            .filter(|(_, shell)| matches!(shell, Shell::Exited { until } if now >= *until))
            .map(|(session_id, _)| *session_id)
            .collect();
        for session_id in drained {
            self.end(session_id, TerminalExitReason::Exited, replies);
        }
    }

    /// The held output first, then the shells' queue.
    fn next_event(&mut self) -> Option<Event> {
        if let Some((session_id, data)) = self.held.take() {
            return Some(Event::Output { session_id, data });
        }
        let event = self.events.try_recv().ok()?;
        if let Event::Output { data, .. } = &event {
            self.event_sink.backlog.take(data.len());
        }
        Some(event)
    }

    /// The part of `data` the session's credit covers; the rest is held.
    /// None for a session that has ended.
    fn take_credit(&mut self, session_id: u32, mut data: Vec<u8>) -> Option<Vec<u8>> {
        let credit = self.credit.get_mut(&session_id)?;
        if data.len() > *credit {
            self.held = Some((session_id, data.split_off(*credit)));
        }
        *credit -= data.len();
        Some(data)
    }

    fn open(&mut self, session_id: u32, size: TerminalSize) -> TerminalOpenStatus {
        if !self.shell {
            return TerminalOpenStatus::Unsupported;
        }
        if self.sessions.contains_key(&session_id) {
            return TerminalOpenStatus::Failed;
        }
        if self.sessions.len() >= MAX_SESSIONS {
            return TerminalOpenStatus::Busy;
        }
        let size = if size.rows == 0 || size.cols == 0 {
            DEFAULT_SIZE
        } else {
            size
        };
        match spawn(session_id, size, &self.event_sink) {
            Ok(session) => {
                self.sessions.insert(session_id, Shell::Running(session));
                self.credit.insert(session_id, TERMINAL_OUTPUT_WINDOW);
                TerminalOpenStatus::Ok
            }
            Err(err) => {
                eprintln!(
                    "ipkvm-os-services: terminal {session_id} could not start a shell: {err}"
                );
                TerminalOpenStatus::Failed
            }
        }
    }

    fn end(
        &mut self,
        session_id: u32,
        reason: TerminalExitReason,
        replies: &mut Vec<TerminalReply>,
    ) {
        self.credit.remove(&session_id);
        if self
            .held
            .as_ref()
            .is_some_and(|(held, _)| *held == session_id)
        {
            self.held = None;
        }
        if self.sessions.remove(&session_id).is_some() {
            replies.push(TerminalReply::Exited { session_id, reason });
        }
    }
}

#[cfg(test)]
impl Terminals {
    /// A session that takes output, without a shell: the returned function
    /// prints into it as a shell's reader does, and is false once the
    /// terminals are gone.
    pub fn printing(&mut self, session_id: u32) -> impl Fn(&[u8]) -> bool {
        self.credit.insert(session_id, TERMINAL_OUTPUT_WINDOW);
        let sink = self.event_sink.clone();
        move |data| {
            sink.send(Event::Output {
                session_id,
                data: data.to_vec(),
            })
        }
    }
}

impl Drop for Terminals {
    /// A shell's reader that waits for room in the queue ends.
    fn drop(&mut self) {
        self.event_sink.backlog.close();
    }
}

fn pty_size(size: TerminalSize) -> PtySize {
    PtySize {
        rows: size.rows,
        cols: size.cols,
        pixel_width: 0,
        pixel_height: 0,
    }
}

type Child = Box<dyn portable_pty::Child + Send + Sync>;

/// Starts a shell on a new pty with its three threads: one reads output, one
/// writes keystrokes, and one waits for the shell to exit.
fn spawn(
    session_id: u32,
    size: TerminalSize,
    events: &EventSink,
) -> Result<Session, Box<dyn std::error::Error>> {
    let pair = native_pty_system().openpty(pty_size(size))?;
    let child = pair.slave.spawn_command(shell_command())?;
    // The slave belongs to the child; holding it here keeps the reader from
    // seeing EOF when the shell exits.
    drop(pair.slave);
    let reader = pair.master.try_clone_reader()?;
    let writer = pair.master.take_writer()?;
    let named = |role: &str| thread::Builder::new().name(format!("terminal-{session_id}-{role}"));
    let sink = events.clone();
    named("read").spawn(move || read_output(session_id, reader, sink))?;
    let (closed, session_closed) = mpsc::channel::<()>();
    let sink = events.clone();
    named("wait").spawn(move || {
        wait_for_exit(child, session_closed);
        sink.send(Event::ChildExited { session_id });
    })?;
    let (input, keystrokes) = mpsc::sync_channel::<Vec<u8>>(INPUT_QUEUE);
    named("write").spawn(move || write_input(writer, keystrokes))?;
    Ok(Session {
        master: Some(pair.master),
        input,
        _closed: closed,
    })
}

/// Forwards what the shell prints until the pty closes.
fn read_output(session_id: u32, mut reader: Box<dyn Read + Send>, sink: EventSink) {
    let mut buffer = vec![0; READ_CHUNK];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let data = buffer[..n].to_vec();
                if !sink.send(Event::Output { session_id, data }) {
                    return;
                }
            }
        }
    }
    sink.send(Event::Eof { session_id });
}

fn write_input(mut writer: Box<dyn Write + Send>, keystrokes: mpsc::Receiver<Vec<u8>>) {
    for data in keystrokes {
        if writer.write_all(&data).is_err() {
            break;
        }
    }
}

/// Reaps the shell, and ends it when the session closes first: a hangup, as
/// a closing terminal window sends so the shell passes it on to its jobs,
/// then a kill for a shell that ignores it. The child is reaped here and
/// only here, so no signal reaches a pid the OS has given to another
/// process.
fn wait_for_exit(mut child: Child, session_closed: mpsc::Receiver<()>) {
    let exited = |child: &mut Child| !matches!(child.try_wait(), Ok(None));
    while !exited(&mut child) {
        if session_closed.recv_timeout(WAIT_POLL) != Err(mpsc::RecvTimeoutError::Timeout) {
            break;
        }
    }
    if exited(&mut child) {
        return;
    }
    #[cfg(unix)]
    if let Some(pid) = child.process_id() {
        unsafe { libc::kill(pid as libc::pid_t, libc::SIGHUP) };
    }
    let deadline = Instant::now() + HANGUP_GRACE;
    while !exited(&mut child) && Instant::now() < deadline {
        thread::sleep(WAIT_POLL);
    }
    if !exited(&mut child) {
        let _ = child.kill();
        let _ = child.wait();
    }
}

/// The user's own login shell, started the way a terminal window starts it,
/// in the home directory.
#[cfg(not(windows))]
fn shell_command() -> CommandBuilder {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
    let mut command = CommandBuilder::new(shell);
    command.arg("-l");
    command.env("TERM", "xterm-256color");
    if let Ok(home) = std::env::var("HOME") {
        command.cwd(home);
    }
    command
}

#[cfg(windows)]
fn shell_command() -> CommandBuilder {
    let mut command = CommandBuilder::new("powershell.exe");
    command.arg("-NoLogo");
    if let Ok(home) = std::env::var("USERPROFILE") {
        command.cwd(home);
    }
    command
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn open(terminals: &mut Terminals, session_id: u32) -> Vec<TerminalReply> {
        let mut replies = Vec::new();
        terminals.handle(
            ControlRecord::TerminalOpen {
                session_id,
                size: TerminalSize { rows: 0, cols: 0 },
            },
            &mut replies,
        );
        replies
    }

    /// Polls until `done` accepts the replies gathered so far. Session 1's
    /// cursor-position queries are answered the way xterm.js answers them:
    /// PowerShell asks before it draws anything and waits for the reply.
    fn wait_for(
        terminals: &mut Terminals,
        done: impl Fn(&[TerminalReply]) -> bool,
    ) -> Vec<TerminalReply> {
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut replies = Vec::new();
        let mut answered = 0;
        while !done(&replies) {
            assert!(
                Instant::now() < deadline,
                "timed out; replies so far: {replies:?}"
            );
            terminals.poll(usize::MAX, &mut replies);
            let queries = output(&replies)
                .windows(4)
                .filter(|window| window == b"\x1b[6n")
                .count();
            for _ in answered..queries {
                terminals.handle(
                    ControlRecord::TerminalInput {
                        session_id: 1,
                        data: b"\x1b[1;1R".to_vec(),
                    },
                    &mut Vec::new(),
                );
            }
            answered = queries;
            thread::sleep(Duration::from_millis(20));
        }
        replies
    }

    fn output(replies: &[TerminalReply]) -> Vec<u8> {
        replies
            .iter()
            .filter_map(|reply| match reply {
                TerminalReply::Output { data, .. } => Some(data.as_slice()),
                _ => None,
            })
            .flatten()
            .copied()
            .collect()
    }

    fn exited(replies: &[TerminalReply]) -> Option<TerminalExitReason> {
        replies.iter().find_map(|reply| match reply {
            TerminalReply::Exited { reason, .. } => Some(*reason),
            _ => None,
        })
    }

    #[test]
    fn shell_echoes_input_and_reports_its_exit() {
        let wakes = Arc::new(AtomicUsize::new(0));
        let counter = wakes.clone();
        let mut terminals = Terminals::new(true, move || {
            counter.fetch_add(1, Ordering::Relaxed);
        });
        assert_eq!(
            open(&mut terminals, 1),
            [TerminalReply::Opened {
                session_id: 1,
                status: TerminalOpenStatus::Ok
            }]
        );
        let mut replies = Vec::new();
        terminals.handle(
            ControlRecord::TerminalInput {
                session_id: 1,
                data: b"echo os-services-marker\rexit\r".to_vec(),
            },
            &mut replies,
        );
        assert!(replies.is_empty());
        let replies = wait_for(&mut terminals, |replies| exited(replies).is_some());
        assert_eq!(exited(&replies), Some(TerminalExitReason::Exited));
        let printed = String::from_utf8_lossy(&output(&replies)).into_owned();
        assert!(
            printed.contains("os-services-marker"),
            "shell output: {printed:?}"
        );
        assert!(terminals.sessions.is_empty());
        // Each event the shell's threads queued woke the session's loop.
        assert!(wakes.load(Ordering::Relaxed) > 0);
    }

    /// A process the shell leaves behind keeps the pty open; the session must
    /// not wait for it. The subshell keeps it out of the shell's job table,
    /// so zsh does not refuse the exit.
    #[cfg(unix)]
    #[test]
    fn exit_with_a_process_left_behind_ends_the_session() {
        let mut terminals = Terminals::new(true, || {});
        open(&mut terminals, 1);
        terminals.handle(
            ControlRecord::TerminalInput {
                session_id: 1,
                data: b"(sleep 30 &)\rexit\r".to_vec(),
            },
            &mut Vec::new(),
        );
        let replies = wait_for(&mut terminals, |replies| exited(replies).is_some());
        assert_eq!(exited(&replies), Some(TerminalExitReason::Exited));
    }

    #[test]
    fn close_ends_a_session_and_withdrawal_ends_them_all() {
        let mut terminals = Terminals::new(true, || {});
        open(&mut terminals, 1);
        open(&mut terminals, 2);
        let mut replies = Vec::new();
        terminals.handle(ControlRecord::TerminalClose { session_id: 1 }, &mut replies);
        assert_eq!(
            replies,
            [TerminalReply::Exited {
                session_id: 1,
                reason: TerminalExitReason::Closed
            }]
        );
        // The shell's own exit after the close is not reported again.
        thread::sleep(Duration::from_millis(200));
        let replies = wait_for(&mut terminals, |_| true);
        assert!(exited(&replies).is_none(), "{replies:?}");

        let mut replies = Vec::new();
        terminals.set_shell(false, &mut replies);
        assert_eq!(
            replies,
            [
                TerminalReply::Exited {
                    session_id: 2,
                    reason: TerminalExitReason::Closed
                },
                TerminalReply::Capabilities { shell: false },
            ]
        );
        assert_eq!(
            open(&mut terminals, 3),
            [TerminalReply::Opened {
                session_id: 3,
                status: TerminalOpenStatus::Unsupported
            }]
        );
    }

    #[test]
    fn output_waits_for_budget() {
        let mut terminals = Terminals::new(true, || {});
        open(&mut terminals, 1);
        // Without budget nothing is forwarded, however long the shell has
        // been printing.
        let mut replies = Vec::new();
        for _ in 0..25 {
            terminals.poll(0, &mut replies);
            thread::sleep(Duration::from_millis(20));
        }
        assert!(replies.is_empty(), "{replies:?}");
        // With budget the queued prompt comes through.
        let replies = wait_for(&mut terminals, |replies| !output(replies).is_empty());
        assert!(!output(&replies).is_empty());
    }

    #[test]
    fn output_waits_for_credit() {
        let mut terminals = Terminals::new(true, || {});
        terminals.credit.insert(7, TERMINAL_OUTPUT_WINDOW);
        let printed = 3 * (TERMINAL_OUTPUT_WINDOW / 2 + 1);
        for _ in 0..3 {
            let data = vec![b'x'; TERMINAL_OUTPUT_WINDOW / 2 + 1];
            terminals.event_sink.send(Event::Output {
                session_id: 7,
                data,
            });
        }
        // Each poll comes an interval after the one before, so none waits.
        let mut now = Instant::now();
        let mut forwarded = |terminals: &mut Terminals| {
            now += OUTPUT_INTERVAL;
            let mut replies = Vec::new();
            terminals.poll_at(now, usize::MAX, &mut replies);
            output(&replies).len()
        };
        // The window goes out, then nothing until the KVM returns credit.
        assert_eq!(forwarded(&mut terminals), TERMINAL_OUTPUT_WINDOW);
        assert_eq!(forwarded(&mut terminals), 0);
        let credit = |terminals: &mut Terminals, bytes| {
            let record = ControlRecord::TerminalCredit {
                session_id: 7,
                bytes,
            };
            terminals.handle(record, &mut Vec::new());
        };
        credit(&mut terminals, 1000);
        assert_eq!(forwarded(&mut terminals), 1000);
        credit(&mut terminals, 1 << 20);
        assert_eq!(
            forwarded(&mut terminals),
            printed - TERMINAL_OUTPUT_WINDOW - 1000
        );
    }

    #[test]
    fn output_that_follows_a_forward_collects_into_one_message() {
        let mut terminals = Terminals::new(true, || {});
        let print = terminals.printing(7);
        let start = Instant::now();
        let poll_at = |terminals: &mut Terminals, after: Duration| {
            let mut replies = Vec::new();
            terminals.poll_at(start + after, usize::MAX, &mut replies);
            replies
        };
        // The first output after a pause goes out with the poll that finds it.
        print(b"x");
        assert_eq!(output(&poll_at(&mut terminals, Duration::ZERO)), b"x");
        assert_eq!(terminals.output_due_in(), None);
        // A program that prints line by line: every read is one line.
        for line in 0..100 {
            print(format!("{line}\r\n").as_bytes());
        }
        assert!(poll_at(&mut terminals, OUTPUT_INTERVAL / 2).is_empty());
        assert!(terminals.output_due_in().is_some());
        let replies = poll_at(&mut terminals, OUTPUT_INTERVAL);
        assert_eq!(replies.len(), 1, "{replies:?}");
        assert!(output(&replies).ends_with(b"98\r\n99\r\n"));
        assert_eq!(terminals.output_due_in(), None);
        // After a pause the next output goes out at once again.
        print(b"y");
        assert_eq!(output(&poll_at(&mut terminals, 3 * OUTPUT_INTERVAL)), b"y");
    }

    #[test]
    fn output_that_the_budget_leaves_is_due_when_the_interval_ends() {
        let mut terminals = Terminals::new(true, || {});
        let print = terminals.printing(7);
        for _ in 0..3 {
            print(b"0123456789");
        }
        let start = Instant::now();
        let mut replies = Vec::new();
        terminals.poll_at(start, 10, &mut replies);
        assert_eq!(output(&replies).len(), 10);
        assert!(terminals.output_due_in().is_some());
        // Without budget the next poll forwards nothing and asks for no
        // further one: the caller's own cadence takes over.
        terminals.poll_at(start + OUTPUT_INTERVAL, 0, &mut replies);
        assert_eq!(output(&replies).len(), 10);
        assert_eq!(terminals.output_due_in(), None);
        terminals.poll_at(start + OUTPUT_INTERVAL, usize::MAX, &mut replies);
        assert_eq!(output(&replies).len(), 30);
        assert_eq!(terminals.output_due_in(), None);
    }

    #[test]
    fn a_shell_that_outruns_the_session_waits_for_room_in_the_queue() {
        let mut terminals = Terminals::new(true, || {});
        let print = terminals.printing(7);
        let line = [b'x'; 100];
        let lines = 2 * OUTPUT_QUEUE / line.len();
        let printed = Arc::new(AtomicUsize::new(0));
        let count = printed.clone();
        let shell = thread::spawn(move || {
            while count.load(Ordering::Relaxed) < lines && print(&line) {
                count.fetch_add(1, Ordering::Relaxed);
            }
        });
        // The queue takes lines up to its limit in bytes, and the shell's
        // next write waits.
        let deadline = Instant::now() + Duration::from_secs(20);
        while printed.load(Ordering::Relaxed) < OUTPUT_QUEUE / line.len() {
            assert!(Instant::now() < deadline, "the queue did not fill");
            thread::sleep(Duration::from_millis(1));
        }
        thread::sleep(Duration::from_millis(50));
        assert_eq!(printed.load(Ordering::Relaxed), OUTPUT_QUEUE / line.len());
        // Polls with credit make room, and the shell prints the rest.
        let mut now = Instant::now();
        let mut forwarded = 0;
        while forwarded < lines * line.len() {
            assert!(Instant::now() < deadline, "forwarded {forwarded}");
            now += OUTPUT_INTERVAL;
            let mut replies = Vec::new();
            terminals.poll_at(now, usize::MAX, &mut replies);
            let bytes = output(&replies).len();
            forwarded += bytes;
            let record = ControlRecord::TerminalCredit {
                session_id: 7,
                bytes: bytes as u32,
            };
            terminals.handle(record, &mut Vec::new());
            thread::yield_now();
        }
        shell.join().unwrap();
        assert_eq!(forwarded, lines * line.len());
    }

    #[test]
    fn a_shell_that_waits_for_room_ends_with_the_session() {
        let mut terminals = Terminals::new(true, || {});
        let print = terminals.printing(7);
        let shell = thread::spawn(move || while print(&[b'x'; 1024]) {});
        thread::sleep(Duration::from_millis(50));
        drop(terminals);
        shell.join().unwrap();
    }

    #[test]
    fn refuses_more_sessions_than_the_limit() {
        let mut terminals = Terminals::new(true, || {});
        for session_id in 1..=MAX_SESSIONS as u32 {
            assert_eq!(
                open(&mut terminals, session_id),
                [TerminalReply::Opened {
                    session_id,
                    status: TerminalOpenStatus::Ok
                }]
            );
        }
        assert_eq!(
            open(&mut terminals, 99),
            [TerminalReply::Opened {
                session_id: 99,
                status: TerminalOpenStatus::Busy
            }]
        );
    }
}
