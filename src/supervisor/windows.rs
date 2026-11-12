//! Stop requests on Windows. The sender has no console, so no console
//! control event reaches it, and a process that Windows ends sends no
//! session.close: the KVM then keeps the session until its liveness limit.
//! Two things reach the running sender instead, and each sets the flag that
//! ends the session cleanly:
//!
//! - A named event, which `--stop` sets. The event is global, so a command
//!   in another Windows session, such as an SSH login, finds it, and its
//!   name ends in the user's ID, as the scheduled task's name does.
//!   Every account can create a global event, so the sender uses only an
//!   event that its own account made.
//! - A window that is never shown. Task Scheduler ends a task by sending
//!   WM_CLOSE to its windows, and ends the process about a second later.
//!   Windows sends WM_ENDSESSION to every top-level window when the user
//!   signs out or the computer shuts down.

use std::{
    cell::RefCell,
    io, ptr,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    },
    thread,
    time::{Duration, Instant},
};
use winapi::{
    shared::{
        minwindef::{LPARAM, LRESULT, UINT, WPARAM},
        sddl::ConvertSidToStringSidW,
        windef::HWND,
        winerror::ERROR_ALREADY_EXISTS,
    },
    um::{
        accctrl::SE_KERNEL_OBJECT,
        aclapi::GetSecurityInfo,
        handleapi::CloseHandle,
        libloaderapi::GetModuleHandleW,
        processthreadsapi::{GetCurrentProcess, OpenProcessToken},
        securitybaseapi::{EqualSid, GetTokenInformation},
        synchapi::{CreateEventW, OpenEventW, SetEvent, WaitForSingleObject},
        winbase::{LocalFree, INFINITE, WAIT_OBJECT_0},
        winnt::{
            TokenOwner, TokenUser, EVENT_MODIFY_STATE, HANDLE, OWNER_SECURITY_INFORMATION, PSID,
            SYNCHRONIZE, TOKEN_INFORMATION_CLASS, TOKEN_QUERY,
        },
        winuser::{
            CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, RegisterClassW,
            WM_CLOSE, WM_ENDSESSION, WNDCLASSW,
        },
    },
};

/// How long `--stop` waits for the sender to exit. The scheduled task's
/// script and the systemd unit allow a stop the same time.
const STOP_TIMEOUT: Duration = Duration::from_secs(10);
/// How often `--stop` looks whether the sender has exited.
const EXIT_POLL: Duration = Duration::from_millis(20);
/// How long the answer to WM_ENDSESSION waits for the session to close.
/// Windows may end the process once the answer returns, and waits 5 seconds
/// for the answer before it tells the user that the program does not respond.
const END_SESSION_WAIT: Duration = Duration::from_secs(3);

/// A kernel handle that closes when dropped.
struct Handle(HANDLE);

// A kernel handle is valid on every thread of the process.
unsafe impl Send for Handle {}

impl Drop for Handle {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain([0]).collect()
}

/// Sets `shutdown` when the sender is asked to stop: by `--stop`, by Task
/// Scheduler ending the task, or by a sign-out or shutdown. The event and
/// the window work without each other, so when one cannot be made, the
/// other still takes its requests.
pub fn watch_stop_requests(shutdown: &Arc<AtomicBool>) {
    match stop_event_name().and_then(|name| create_event(&name)) {
        Ok(event) => watch_stop_event(event, shutdown.clone()),
        Err(error) => eprintln!("ipkvm-os-services: --stop cannot reach this service: {error}"),
    }
    if let Err(error) = stop_window(shutdown.clone()) {
        eprintln!(
            "ipkvm-os-services: Task Scheduler and a sign-out end this service without \
             closing its session: {error}"
        );
    }
}

/// Asks this user's running sender to end its session and exit, and waits
/// until it has exited. Succeeds at once when no sender runs.
pub fn stop_sender() -> Result<(), Box<dyn std::error::Error>> {
    let name = stop_event_name()?;
    if request_stop(&name)? && !wait_until_gone(&name, STOP_TIMEOUT)? {
        return Err(format!(
            "the running service did not stop within {} seconds",
            STOP_TIMEOUT.as_secs()
        )
        .into());
    }
    Ok(())
}

/// The name of the stop event of this user's sender.
fn stop_event_name() -> io::Result<Vec<u16>> {
    Ok(wide(&format!(
        "Global\\ipkvm-os-services-stop-{}",
        user_sid()?
    )))
}

/// The security ID of the user this process runs as, in its string form.
fn user_sid() -> io::Result<String> {
    with_token_sid(TokenUser, |sid| unsafe {
        let mut text = ptr::null_mut();
        if ConvertSidToStringSidW(sid, &mut text) == 0 {
            return Err(io::Error::last_os_error());
        }
        let length = (0..).take_while(|&at| *text.add(at) != 0).count();
        let sid = String::from_utf16_lossy(std::slice::from_raw_parts(text, length));
        LocalFree(text.cast());
        Ok(sid)
    })?
}

/// Calls `read` with a security ID of this process: the user it runs as
/// (`TokenUser`), or the owner of the objects it creates (`TokenOwner`).
fn with_token_sid<T>(
    class: TOKEN_INFORMATION_CLASS,
    read: impl FnOnce(PSID) -> T,
) -> io::Result<T> {
    // Both answers start with a pointer to the SID, which follows in the
    // same buffer; a SID has at most 68 bytes.
    let mut buffer = [0u64; 16];
    let mut length = 0;
    unsafe {
        let mut token = ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return Err(io::Error::last_os_error());
        }
        let token = Handle(token);
        let size = std::mem::size_of_val(&buffer) as u32;
        if GetTokenInformation(
            token.0,
            class,
            buffer.as_mut_ptr().cast(),
            size,
            &mut length,
        ) == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(read(*buffer.as_ptr().cast::<PSID>()))
    }
}

/// Creates the stop event, or opens the one that another sender of this
/// user has created. It stays set once set, so every sender that holds it
/// stops. An event that another account made first is refused: that account
/// could set it, and stop every sender of this user at its start.
fn create_event(name: &[u16]) -> io::Result<Handle> {
    let event = unsafe { CreateEventW(ptr::null_mut(), 1, 0, name.as_ptr()) };
    let error = io::Error::last_os_error();
    if event.is_null() {
        return Err(error);
    }
    let event = Handle(event);
    if error.raw_os_error() == Some(ERROR_ALREADY_EXISTS as i32) && !made_by_this_account(&event)? {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "another account made the stop event",
        ));
    }
    Ok(event)
}

/// Whether this account made the event: its owner is the user of this
/// process, or the owner of the objects that this process creates. The two
/// differ in an elevated process of an administrator, whose objects the
/// Administrators group can own, and a sender of the same user that is not
/// elevated makes an event that the user owns.
fn made_by_this_account(event: &Handle) -> io::Result<bool> {
    unsafe {
        let (mut owner, mut descriptor) = (ptr::null_mut(), ptr::null_mut());
        let status = GetSecurityInfo(
            event.0,
            SE_KERNEL_OBJECT,
            OWNER_SECURITY_INFORMATION,
            &mut owner,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            &mut descriptor,
        );
        if status != 0 {
            return Err(io::Error::from_raw_os_error(status as i32));
        }
        let is = |class| with_token_sid(class, |mine| EqualSid(owner, mine) != 0);
        let same = is(TokenUser).and_then(|user| Ok(user || is(TokenOwner)?));
        LocalFree(descriptor.cast());
        same
    }
}

/// Opens the event with this name; None when no process holds one.
fn open_event(name: &[u16], access: u32) -> io::Result<Option<Handle>> {
    let event = unsafe { OpenEventW(access, 0, name.as_ptr()) };
    if !event.is_null() {
        return Ok(Some(Handle(event)));
    }
    match io::Error::last_os_error() {
        error if error.kind() == io::ErrorKind::NotFound => Ok(None),
        error => Err(error),
    }
}

/// Sets `shutdown` when the event is set.
fn watch_stop_event(event: Handle, shutdown: Arc<AtomicBool>) {
    thread::spawn(move || {
        if unsafe { WaitForSingleObject(event.0, INFINITE) } == WAIT_OBJECT_0 {
            shutdown.store(true, Ordering::Relaxed);
        }
        // The event lasts until the process exits: `--stop` takes the end
        // of the event as the exit of the sender.
        std::mem::forget(event);
    });
}

/// Sets the stop event; false when no sender holds one.
fn request_stop(name: &[u16]) -> io::Result<bool> {
    let Some(event) = open_event(name, EVENT_MODIFY_STATE)? else {
        return Ok(false);
    };
    if unsafe { SetEvent(event.0) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(true)
}

/// Waits until no process holds the stop event; false when one still does
/// after `timeout`. Windows removes the event when its last holder exits.
fn wait_until_gone(name: &[u16], timeout: Duration) -> io::Result<bool> {
    let deadline = Instant::now() + timeout;
    while open_event(name, SYNCHRONIZE)?.is_some() {
        if Instant::now() >= deadline {
            return Ok(false);
        }
        thread::sleep(EXIT_POLL);
    }
    Ok(true)
}

thread_local! {
    /// The flag that the window of this thread sets.
    static WINDOW_SHUTDOWN: RefCell<Option<Arc<AtomicBool>>> = const { RefCell::new(None) };
}

/// Creates the window on a thread of its own, which serves its messages
/// for the life of the process. Returns the window's handle.
fn stop_window(shutdown: Arc<AtomicBool>) -> io::Result<usize> {
    let (created, result) = mpsc::channel();
    thread::spawn(move || {
        WINDOW_SHUTDOWN.with(|flag| *flag.borrow_mut() = Some(shutdown));
        let window = create_window();
        let failed = window.is_err();
        let _ = created.send(window);
        if failed {
            return;
        }
        unsafe {
            let mut message = std::mem::zeroed();
            while GetMessageW(&mut message, ptr::null_mut(), 0, 0) > 0 {
                DispatchMessageW(&message);
            }
        }
    });
    result
        .recv()
        .map_err(|_| io::Error::other("the stop window's thread ended"))?
}

/// A top-level window without the visible style: Windows sends WM_CLOSE and
/// WM_ENDSESSION to top-level windows only, whether shown or not.
fn create_window() -> io::Result<usize> {
    let name = wide("ipkvm-os-services");
    unsafe {
        let instance = GetModuleHandleW(ptr::null());
        let mut class: WNDCLASSW = std::mem::zeroed();
        class.lpfnWndProc = Some(window_procedure);
        class.hInstance = instance;
        class.lpszClassName = name.as_ptr();
        // The class is already there when the process has made the window
        // before; creating the window fails if the class is missing.
        RegisterClassW(&class);
        let window = CreateWindowExW(
            0,
            name.as_ptr(),
            name.as_ptr(),
            0,
            0,
            0,
            0,
            0,
            ptr::null_mut(),
            ptr::null_mut(),
            instance,
            ptr::null_mut(),
        );
        if window.is_null() {
            return Err(io::Error::last_os_error());
        }
        Ok(window as usize)
    }
}

unsafe extern "system" fn window_procedure(
    window: HWND,
    message: UINT,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let stop = || {
        WINDOW_SHUTDOWN.with(|flag| {
            if let Some(shutdown) = flag.borrow().as_ref() {
                shutdown.store(true, Ordering::Relaxed);
            }
        });
    };
    match message {
        // The window stays: the process exits when the session has closed.
        WM_CLOSE => {
            stop();
            0
        }
        // The session ends (a zero wparam means that it does not after
        // all). The process exits during the wait, as soon as the session
        // with the KVM has closed.
        WM_ENDSESSION if wparam != 0 => {
            stop();
            thread::sleep(END_SESSION_WAIT);
            0
        }
        _ => DefWindowProcW(window, message, wparam, lparam),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use winapi::um::winuser::{PostMessageW, SendMessageW, WM_QUERYENDSESSION};

    fn set_soon(flag: &AtomicBool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(2);
        while !flag.load(Ordering::Relaxed) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        flag.load(Ordering::Relaxed)
    }

    /// A name of this test process only, so that the test does not stop a
    /// sender that runs on the same computer.
    fn test_event_name() -> Vec<u16> {
        wide(&format!(
            "Local\\ipkvm-os-services-test-{}",
            std::process::id()
        ))
    }

    #[test]
    fn a_stop_request_reaches_the_sender_and_ends_when_the_sender_exits() {
        let name = test_event_name();
        assert!(!request_stop(&name).unwrap(), "no service runs yet");
        assert!(wait_until_gone(&name, Duration::ZERO).unwrap());

        let shutdown = Arc::new(AtomicBool::new(false));
        let event = create_event(&name).unwrap();
        let owner = event.0 as usize;
        watch_stop_event(event, shutdown.clone());
        assert!(!shutdown.load(Ordering::Relaxed));
        assert!(request_stop(&name).unwrap());
        assert!(set_soon(&shutdown));

        // The request is over only when the sender's handle is gone, which
        // Windows closes when the process exits.
        assert!(!wait_until_gone(&name, Duration::from_millis(100)).unwrap());
        unsafe { CloseHandle(owner as HANDLE) };
        assert!(wait_until_gone(&name, Duration::from_secs(2)).unwrap());
    }

    /// An elevated sender finds the event of a sender of the same user
    /// that is not elevated: the user owns that event, also when the
    /// elevated process gives its own objects to the Administrators group.
    #[test]
    fn an_event_that_the_user_owns_is_shared_at_every_elevation() {
        use winapi::{
            shared::sddl::ConvertStringSecurityDescriptorToSecurityDescriptorW,
            um::minwinbase::SECURITY_ATTRIBUTES,
        };
        let name = wide(&format!(
            "Local\\ipkvm-os-services-test-owner-{}",
            std::process::id()
        ));
        let owner = wide(&format!("O:{}", user_sid().unwrap()));
        let mut descriptor = ptr::null_mut();
        let converted = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                owner.as_ptr(),
                1,
                &mut descriptor,
                ptr::null_mut(),
            )
        };
        assert_ne!(converted, 0);
        let mut attributes = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor,
            bInheritHandle: 0,
        };
        let first = Handle(unsafe { CreateEventW(&mut attributes, 1, 0, name.as_ptr()) });
        unsafe { LocalFree(descriptor.cast()) };
        assert!(!first.0.is_null());
        assert!(made_by_this_account(&first).unwrap());
        create_event(&name).unwrap();
    }

    #[test]
    fn a_second_sender_of_this_account_shares_the_stop_event() {
        let name = wide(&format!(
            "Local\\ipkvm-os-services-test-shared-{}",
            std::process::id()
        ));
        let first = create_event(&name).unwrap();
        let second = create_event(&name).unwrap();
        assert!(request_stop(&name).unwrap());
        for event in [first, second] {
            assert_eq!(unsafe { WaitForSingleObject(event.0, 0) }, WAIT_OBJECT_0);
        }
    }

    #[test]
    fn the_stop_event_is_named_after_the_user() {
        let name = String::from_utf16(&stop_event_name().unwrap()).unwrap();
        let sid = name
            .strip_prefix("Global\\ipkvm-os-services-stop-S-1-")
            .and_then(|rest| rest.strip_suffix('\0'))
            .expect(&name);
        assert!(
            sid.split('-').all(|part| part.parse::<u64>().is_ok()),
            "{name}"
        );
    }

    #[test]
    fn task_scheduler_closing_the_window_stops_the_sender() {
        let shutdown = Arc::new(AtomicBool::new(false));
        let window = stop_window(shutdown.clone()).unwrap() as HWND;
        assert_ne!(unsafe { PostMessageW(window, WM_CLOSE, 0, 0) }, 0);
        assert!(set_soon(&shutdown));
    }

    #[test]
    fn a_session_end_stops_the_sender_and_a_cancelled_one_does_not() {
        let shutdown = Arc::new(AtomicBool::new(false));
        let window = stop_window(shutdown.clone()).unwrap() as HWND;
        // The sender does not refuse a sign-out. SendMessageW returns when
        // the window has handled the message.
        assert_ne!(unsafe { SendMessageW(window, WM_QUERYENDSESSION, 0, 0) }, 0);
        unsafe { SendMessageW(window, WM_ENDSESSION, 0, 0) };
        assert!(!shutdown.load(Ordering::Relaxed));
        assert_ne!(unsafe { PostMessageW(window, WM_ENDSESSION, 1, 0) }, 0);
        assert!(set_soon(&shutdown));
    }
}
