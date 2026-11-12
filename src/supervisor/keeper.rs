//! The service on a Linux computer without systemd. systemd starts the
//! unit, starts it again after a failure and after an update, and keeps its
//! log. Without systemd, the autostart entry of the desktop starts the
//! program with `--run`, and that process is the keeper: it runs the service
//! as a child process, starts it again when it ends with a failure or after
//! an update, and points the output of both at a log file.
//!
//! One keeper runs the service for a user. It holds a lock on a file in the
//! user's state folder, `~/.local/state/ipkvm-os-services`. The kernel frees
//! the lock when the keeper's process ends, and it names the process that
//! holds the lock, so `stop` finds the keeper without a stored process ID.
//!
//! A new keeper takes the place of one that runs. Nothing ends a keeper at
//! a sign-out, so the keeper of an earlier session can still run at the
//! next sign-in, with the display variables of that session. The keeper
//! that the new session starts has the variables of the new one. While it
//! waits for its place, it holds a second lock that all keepers share, so
//! `stop` also finds a keeper that does not run the service yet.

use std::{
    error::Error,
    fs::{self, File},
    io::{self, IsTerminal},
    os::{fd::AsRawFd, unix::process::CommandExt},
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Stdio},
    sync::{
        atomic::{AtomicBool, AtomicI32, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};

/// EX_TEMPFAIL: the service ends with this status after it installed an
/// update, and the keeper starts the new program at once.
pub const RESTART_STATUS: i32 = 75;

/// The variable that the keeper sets for the service, so that the service
/// serves the KVM and does not become a keeper itself.
const KEPT: &str = "IPKVM_OS_SERVICES_KEEPER";

const FOLDER: &str = "ipkvm-os-services";
/// The time between a failure of the service and its next start, as
/// RestartSec in the unit.
const RESTART_DELAY: Duration = Duration::from_secs(2);
/// The time that the service gets to end its session, as TimeoutStopSec in
/// the unit.
const STOP_TIMEOUT: Duration = Duration::from_secs(10);
const POLL: Duration = Duration::from_millis(100);

/// Whether systemd runs this computer. The folder exists only then (see
/// sd_booted(3)).
pub fn systemd() -> bool {
    Path::new("/run/systemd/system").is_dir()
}

/// Whether a keeper started this process.
pub fn kept() -> bool {
    std::env::var_os(KEPT).is_some()
}

/// Whether this process, started with `--run`, is the keeper.
pub fn needed() -> bool {
    !systemd() && !kept()
}

/// The log of the keeper and the service.
pub fn log_path() -> Result<PathBuf, Box<dyn Error>> {
    Ok(Files::of_user()?.log)
}

struct Files {
    lock: PathBuf,
    log: PathBuf,
}

impl Files {
    fn of_user() -> Result<Self, Box<dyn Error>> {
        let folder = dirs::state_dir()
            .ok_or("no user state directory")?
            .join(FOLDER);
        Ok(Self {
            lock: folder.join("service.lock"),
            log: folder.join("service.log"),
        })
    }
}

/// Runs the service until it ends successfully or this process is asked to
/// stop; returns the exit status of the process.
pub fn run() -> i32 {
    let kept = Files::of_user().and_then(|files| {
        let program = program()?;
        let stop = Arc::new(AtomicBool::new(false));
        let service = Arc::new(AtomicI32::new(0));
        let (asked, running) = (stop.clone(), service.clone());
        ctrlc::set_handler(move || end_service(&asked, &running))?;
        // A keeper that a terminal started writes there.
        let log = (!io::stderr().is_terminal()).then_some(files.log.as_path());
        keep(&files.lock, log, &program, RESTART_DELAY, &stop, &service)
    });
    match kept {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("ipkvm-os-services: {error}");
            1
        }
    }
}

/// The program that the keeper starts again and again. Each one has a
/// path that stays the same when an update replaces it: the program in the
/// user's folder, or the AppImage that this program runs from. The program
/// file inside a mounted AppImage is still the earlier build after an
/// update. A plain build has no update.
fn program() -> Result<PathBuf, Box<dyn Error>> {
    if let Some(folder) = super::installed::current() {
        return Ok(super::installed::program(&folder));
    }
    match std::env::var_os("APPIMAGE") {
        Some(image) => Ok(image.into()),
        None => Ok(std::env::current_exe()?),
    }
}

/// The stop request: the service gets SIGTERM, and SIGKILL when it has not
/// ended after STOP_TIMEOUT.
fn end_service(stop: &AtomicBool, service: &AtomicI32) {
    stop.store(true, Ordering::SeqCst);
    let asked = service.load(Ordering::SeqCst);
    if asked == 0 {
        return;
    }
    signal(asked, libc::SIGTERM);
    thread::sleep(STOP_TIMEOUT);
    if service.load(Ordering::SeqCst) == asked {
        signal(asked, libc::SIGKILL);
    }
}

fn signal(process: i32, signal: i32) {
    unsafe { libc::kill(process, signal) };
}

/// Runs `program` again and again: at once after RESTART_STATUS, and after
/// `delay` when it failed. Ends when the program ends successfully or when
/// `stop` is set. A keeper that runs the service already stops first. `service` has the process ID of the running program, for
/// the stop request. With `log`, the output goes there.
fn keep(
    lock: &Path,
    log: Option<&Path>,
    program: &Path,
    delay: Duration,
    stop: &AtomicBool,
    service: &AtomicI32,
) -> Result<(), Box<dyn Error>> {
    let locks = Locks::open(lock)?;
    locks.enter()?;
    if !locks.take()? {
        stop_runner(&locks)?;
        // Another start can take the place first; it is as new as this one.
        if !locks.take()? {
            return Ok(());
        }
    }
    // The service moves a large log aside (see `limit_log`), so the keeper
    // opens the log again before each of its own lines.
    let point_at_log = || {
        if let Some(log) = log {
            let streams = [libc::STDOUT_FILENO, libc::STDERR_FILENO];
            if let Err(error) = super::point_streams_at(log, &streams) {
                eprintln!("ipkvm-os-services: the log could not be opened: {error}");
            }
        }
    };
    while !stop.load(Ordering::SeqCst) {
        point_at_log();
        let status = match start_service(program, stop, service) {
            Ok(status) => status,
            Err(error) => {
                eprintln!("ipkvm-os-services: the service could not start: {error}");
                sleep_unless_stopped(stop, delay);
                continue;
            }
        };
        if stop.load(Ordering::SeqCst) || status.success() {
            break;
        }
        if status.code() == Some(RESTART_STATUS) {
            continue;
        }
        point_at_log();
        eprintln!("ipkvm-os-services: the service ended ({status}); it starts again");
        sleep_unless_stopped(stop, delay);
    }
    Ok(())
}

/// Runs the service and waits until it ends. The service gets SIGTERM when
/// the keeper dies, so no service runs without a keeper.
fn start_service(program: &Path, stop: &AtomicBool, service: &AtomicI32) -> io::Result<ExitStatus> {
    let mut command = Command::new(program);
    command.arg("--run").env(KEPT, "1");
    let keeper = std::process::id() as libc::pid_t;
    unsafe {
        command.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) < 0 {
                return Err(io::Error::last_os_error());
            }
            // No signal comes for a keeper that died before the line above.
            if libc::getppid() != keeper {
                return Err(io::Error::from_raw_os_error(libc::ESRCH));
            }
            Ok(())
        });
    }
    let mut child = command.spawn()?;
    let process = child.id() as i32;
    service.store(process, Ordering::SeqCst);
    // A stop request that came before the line above found no service.
    if stop.load(Ordering::SeqCst) {
        signal(process, libc::SIGTERM);
    }
    let status = child.wait();
    service.store(0, Ordering::SeqCst);
    status
}

fn sleep_unless_stopped(stop: &AtomicBool, time: Duration) {
    let end = Instant::now() + time;
    while !stop.load(Ordering::SeqCst) && Instant::now() < end {
        thread::sleep(POLL.min(time));
    }
}

/// Starts a keeper for `program`, in place of one that runs. The keeper
/// gets a session of its own, so it outlives the terminal or the dialog
/// that started it.
pub fn start(program: &Path) -> Result<(), Box<dyn Error>> {
    stop()?;
    let files = Files::of_user()?;
    let locks = Locks::open(&files.lock)?;
    let mut command = Command::new(program);
    command
        .arg("--run")
        .env_remove(KEPT)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command.spawn()?;
    let end = Instant::now() + STOP_TIMEOUT;
    while locks.runner()?.is_none() {
        if Instant::now() > end {
            return Err(format!("the service did not start; see {}", files.log.display()).into());
        }
        thread::sleep(POLL);
    }
    Ok(())
}

/// Asks every keeper to stop, and waits until none is left. A keeper stops
/// the service first.
pub fn stop() -> Result<(), Box<dyn Error>> {
    match Locks::existing(&Files::of_user()?.lock)? {
        Some(locks) => stop_keepers(&locks),
        None => Ok(()),
    }
}

/// Stops the keeper that runs the service, and each keeper that waited for
/// it and takes its place, until no keeper is left. A keeper states that it
/// is one before it looks for its place, so a keeper that waits is not
/// missed. Each keeper gets the time that it needs to stop a service that
/// does not end by itself.
fn stop_keepers(locks: &Locks) -> Result<(), Box<dyn Error>> {
    let time = STOP_TIMEOUT + STOP_TIMEOUT / 2;
    let mut end = Instant::now() + time;
    let mut asked = None;
    while locks.keeper()?.is_some() {
        let runner = locks.runner()?;
        if runner.is_some() && runner != asked {
            signal(runner.unwrap_or_default(), libc::SIGTERM);
            asked = runner;
            end = Instant::now() + time;
        }
        if Instant::now() > end {
            return Err("the running service did not stop".into());
        }
        thread::sleep(POLL);
    }
    Ok(())
}

/// Asks the keeper that runs the service to stop, and waits until that
/// process no longer runs it. Another keeper can run it by then.
fn stop_runner(locks: &Locks) -> Result<(), Box<dyn Error>> {
    let Some(runner) = locks.runner()? else {
        return Ok(());
    };
    signal(runner, libc::SIGTERM);
    let end = Instant::now() + STOP_TIMEOUT + STOP_TIMEOUT / 2;
    while locks.runner()? == Some(runner) {
        if Instant::now() > end {
            return Err("the running service did not stop".into());
        }
        thread::sleep(POLL);
    }
    Ok(())
}

/// The byte of the lock file that the keeper that runs the service locks.
const RUNS: libc::off_t = 0;
/// The byte that every keeper locks, also one that waits for its place.
/// Keepers share this lock.
const PRESENT: libc::off_t = 1;

/// The lock file, open in this process. Its locks are POSIX record locks:
/// unlike a flock lock, the kernel reports the process that holds one. A
/// process loses its record locks on a file when it closes any descriptor
/// of that file, so a process opens the lock file one time.
struct Locks(File);

impl Locks {
    /// Opens the lock file, and creates it with its folder.
    fn open(path: &Path) -> io::Result<Self> {
        if let Some(folder) = path.parent() {
            fs::create_dir_all(folder)?;
        }
        let file = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        Ok(Self(file))
    }

    /// Opens the lock file; None when no keeper made one.
    fn existing(path: &Path) -> io::Result<Option<Self>> {
        match File::options().read(true).write(true).open(path) {
            Ok(file) => Ok(Some(Self(file))),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// States that this process is a keeper, for its lifetime.
    fn enter(&self) -> io::Result<()> {
        self.set(PRESENT, libc::F_RDLCK).map(drop)
    }

    /// Takes the right to run the service, for the lifetime of the process;
    /// false when another keeper has it.
    fn take(&self) -> io::Result<bool> {
        self.set(RUNS, libc::F_WRLCK)
    }

    /// The other process that runs the service, if one does.
    fn runner(&self) -> io::Result<Option<i32>> {
        self.holder(RUNS)
    }

    /// Another process that is a keeper, if one is: the one that runs the
    /// service, or one that waits for its place.
    fn keeper(&self) -> io::Result<Option<i32>> {
        self.holder(PRESENT)
    }

    fn set(&self, byte: libc::off_t, kind: libc::c_int) -> io::Result<bool> {
        match record_lock(&self.0, libc::F_SETLK, kind, byte) {
            Ok(_) => Ok(true),
            Err(error) if matches!(error.raw_os_error(), Some(libc::EAGAIN | libc::EACCES)) => {
                Ok(false)
            }
            Err(error) => Err(error),
        }
    }

    /// A process that holds a lock on the byte. The locks of this process
    /// itself do not count: the kernel reports only the locks of others.
    fn holder(&self, byte: libc::off_t) -> io::Result<Option<i32>> {
        let lock = record_lock(&self.0, libc::F_GETLK, libc::F_WRLCK, byte)?;
        Ok((lock.l_type != libc::F_UNLCK as libc::c_short).then_some(lock.l_pid))
    }
}

/// Sets a lock of `kind` on one byte of the file, or with F_GETLK asks
/// which lock of another process prevents it.
fn record_lock(
    file: &File,
    command: libc::c_int,
    kind: libc::c_int,
    byte: libc::off_t,
) -> io::Result<libc::flock> {
    let mut lock: libc::flock = unsafe { std::mem::zeroed() };
    lock.l_type = kind as libc::c_short;
    lock.l_whence = libc::SEEK_SET as libc::c_short;
    lock.l_start = byte;
    lock.l_len = 1;
    if unsafe { libc::fcntl(file.as_raw_fd(), command, &mut lock) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(lock)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{ffi::CString, os::unix::fs::PermissionsExt};

    /// A temporary folder that is removed however the test ends.
    struct Root(PathBuf);

    impl Root {
        fn new(name: &str) -> Self {
            let root = std::env::temp_dir().join(format!("keeper-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(&root).unwrap();
            Self(root)
        }
    }

    impl Drop for Root {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// A program that adds a line to `runs` each time it starts, and ends
    /// with the status that `statuses` has for that start.
    fn program(root: &Path, statuses: &[i32]) -> PathBuf {
        let statuses: Vec<String> = statuses.iter().map(i32::to_string).collect();
        let script = format!(
            "#!/bin/sh\necho run >> {runs}\nset -- {statuses}\nshift $(($(wc -l < {runs}) - 1))\nexit $1\n",
            runs = root.join("runs").display(),
            statuses = statuses.join(" "),
        );
        let program = root.join("service");
        fs::write(&program, script).unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
        program
    }

    fn runs(root: &Path) -> usize {
        fs::read_to_string(root.join("runs"))
            .map(|runs| runs.lines().count())
            .unwrap_or(0)
    }

    #[test]
    fn the_service_starts_again_after_an_update_and_after_a_failure_until_it_ends_successfully() {
        let root = Root::new("restart");
        let program = program(&root.0, &[RESTART_STATUS, 1, 0, 1]);
        keep(
            &root.0.join("lock"),
            None,
            &program,
            Duration::from_millis(10),
            &AtomicBool::new(false),
            &AtomicI32::new(0),
        )
        .unwrap();
        assert_eq!(runs(&root.0), 3);
    }

    /// Runs a process that is a keeper: it states that it is one, takes
    /// the place of the keeper that runs the service as soon as that place
    /// is free, and holds it until it gets SIGTERM. With `later`, it takes
    /// the place that time after it became free, as a keeper that looks at
    /// the lock at intervals. After the fork, the child makes only system
    /// calls. It ends with the thread of its test, so a test that fails
    /// leaves no process that keeps the output of the test run open.
    fn other_keeper(path: &Path, later: Option<Duration>) -> i32 {
        let name = CString::new(path.to_str().unwrap()).unwrap();
        let later = later.map(|later| later.as_micros() as libc::c_uint);
        let child = unsafe { libc::fork() };
        assert!(child >= 0);
        if child == 0 {
            unsafe {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
                let file = libc::open(name.as_ptr(), libc::O_RDWR | libc::O_CREAT, 0o600);
                let lock = |command: libc::c_int, kind: libc::c_int, byte: libc::off_t| {
                    let mut lock: libc::flock = std::mem::zeroed();
                    lock.l_type = kind as libc::c_short;
                    lock.l_start = byte;
                    lock.l_len = 1;
                    let result = libc::fcntl(file, command, &mut lock);
                    (result, lock.l_type)
                };
                if file < 0 || lock(libc::F_SETLK, libc::F_RDLCK, PRESENT).0 < 0 {
                    libc::_exit(1);
                }
                let mut command = libc::F_SETLKW;
                if let Some(later) = later {
                    while lock(libc::F_GETLK, libc::F_WRLCK, RUNS).1
                        != libc::F_UNLCK as libc::c_short
                    {
                        libc::usleep(5_000);
                    }
                    libc::usleep(later);
                    command = libc::F_SETLK;
                }
                if lock(command, libc::F_WRLCK, RUNS).0 < 0 {
                    libc::_exit(1);
                }
                libc::pause();
                libc::_exit(0);
            }
        }
        child
    }

    fn wait_for_runner(locks: &Locks, process: i32) {
        let end = Instant::now() + Duration::from_secs(10);
        while locks.runner().unwrap() != Some(process) {
            assert!(
                Instant::now() < end,
                "process {process} does not run the service"
            );
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn ended_by_sigterm(process: i32) -> bool {
        let mut status = 0;
        assert_eq!(unsafe { libc::waitpid(process, &mut status, 0) }, process);
        libc::WIFSIGNALED(status) && libc::WTERMSIG(status) == libc::SIGTERM
    }

    #[test]
    fn a_new_keeper_takes_the_place_of_the_keeper_that_runs_the_service() {
        let root = Root::new("place");
        let lock = root.0.join("lock");
        let locks = Locks::open(&lock).unwrap();
        assert_eq!(locks.keeper().unwrap(), None);
        let keeper = other_keeper(&lock, None);
        wait_for_runner(&locks, keeper);

        let program = program(&root.0, &[0]);
        keep(
            &lock,
            None,
            &program,
            Duration::from_millis(10),
            &AtomicBool::new(false),
            &AtomicI32::new(0),
        )
        .unwrap();
        assert_eq!(runs(&root.0), 1);

        // The kernel frees the locks when the process is gone, before this
        // process reaps it.
        assert_eq!(locks.keeper().unwrap(), None);
        assert!(ended_by_sigterm(keeper));
    }

    #[test]
    fn the_wait_for_a_keeper_ends_when_another_keeper_takes_its_place() {
        let root = Root::new("overlap");
        let lock = root.0.join("lock");
        let locks = Locks::open(&lock).unwrap();
        let keeper = other_keeper(&lock, None);
        wait_for_runner(&locks, keeper);
        // The second keeper gets the place when the first one ends, so the
        // place is never free.
        let next = other_keeper(&lock, None);
        thread::sleep(Duration::from_millis(100));

        let started = Instant::now();
        stop_runner(&locks).unwrap();
        assert!(started.elapsed() < STOP_TIMEOUT);
        wait_for_runner(&locks, next);

        assert!(ended_by_sigterm(keeper));
        signal(next, libc::SIGTERM);
        assert!(ended_by_sigterm(next));
    }

    #[test]
    fn a_stop_of_the_service_also_stops_a_keeper_that_waits_for_its_place() {
        let root = Root::new("waiting");
        let lock = root.0.join("lock");
        let locks = Locks::open(&lock).unwrap();
        let keeper = other_keeper(&lock, None);
        wait_for_runner(&locks, keeper);
        // The place is free for 400 ms before the waiting keeper takes it.
        let waiting = other_keeper(&lock, Some(POLL * 4));
        thread::sleep(Duration::from_millis(100));

        stop_keepers(&locks).unwrap();
        assert_eq!(locks.keeper().unwrap(), None);
        assert!(ended_by_sigterm(keeper));
        assert!(ended_by_sigterm(waiting));
    }
}
