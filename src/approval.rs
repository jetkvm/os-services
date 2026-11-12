//! The KVMs the user has approved. The sender opens a session only with a
//! KVM whose fingerprint is in the user's approved list. For any other
//! KVM it asks the user in a dialog, and it waits until the user approves,
//! the KVM disconnects, or the process stops. An open session ends when the
//! user removes its KVM from the list.

use crate::protocol::{self, Fingerprint};
use std::{
    cell::Cell,
    error::Error,
    fs, io,
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::{Duration, Instant},
};

/// How often the approved list is read while the sender waits for an
/// approval, and while a session is open.
const POLL: Duration = Duration::from_secs(1);

/// The approved list: one fingerprint per line, in the user's local
/// configuration folder.
#[derive(Clone)]
pub struct PinStore {
    path: PathBuf,
}

impl PinStore {
    pub fn for_user() -> io::Result<Self> {
        let folder = dirs::config_local_dir().ok_or(io::ErrorKind::NotFound)?;
        let path = folder.join("ipkvm-os-services").join("approved-kvms");
        Ok(Self { path })
    }

    /// An empty list in a folder of its own, for a test.
    #[cfg(test)]
    pub fn temporary(name: &str) -> Self {
        let folder = std::env::temp_dir().join(format!("pins-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&folder);
        Self {
            path: folder.join("approved-kvms"),
        }
    }

    /// The approved KVMs. A list that cannot be read as fingerprints is
    /// damaged, and approves no KVM.
    pub fn load(&self) -> io::Result<Vec<Fingerprint>> {
        let text = match fs::read_to_string(&self.path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) if error.kind() == io::ErrorKind::InvalidData => {
                return Err(self.damaged(error))
            }
            text => text?,
        };
        text.lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| Fingerprint::parse(line.trim()).map_err(|error| self.damaged(error)))
            .collect()
    }

    fn damaged(&self, reason: impl std::fmt::Display) -> io::Error {
        let text = format!(
            "the approved list {} is damaged ({reason})",
            self.path.display()
        );
        io::Error::new(io::ErrorKind::InvalidData, text)
    }

    /// The list that a change starts from. A damaged list approves no KVM,
    /// so the change starts from an empty list and replaces the file.
    fn load_for_change(&self) -> io::Result<Vec<Fingerprint>> {
        match self.load() {
            Err(error) if error.kind() == io::ErrorKind::InvalidData => {
                eprintln!("ipkvm-os-services: {error}; a new list replaces it");
                Ok(Vec::new())
            }
            list => list,
        }
    }

    pub fn approve(&self, kvm: &Fingerprint) -> io::Result<()> {
        let mut approved = self.load_for_change()?;
        if !approved.contains(kvm) {
            approved.push(*kvm);
        }
        self.save(&approved)
    }

    /// Removes a KVM; false when it was not approved.
    pub fn forget(&self, kvm: &Fingerprint) -> io::Result<bool> {
        let mut approved = self.load_for_change()?;
        let count = approved.len();
        approved.retain(|pin| pin != kvm);
        self.save(&approved)?;
        Ok(approved.len() != count)
    }

    fn save(&self, approved: &[Fingerprint]) -> io::Result<()> {
        let text: String = approved.iter().map(|pin| format!("{pin}\n")).collect();
        fs::create_dir_all(self.path.parent().expect("the store is in a folder"))?;
        let partial = self.path.with_extension(std::process::id().to_string());
        fs::write(&partial, text)?;
        fs::rename(partial, &self.path)
    }
}

/// The user's approval of the KVM of an open session. `--forget-kvm` runs in
/// another process and changes only the list, so the session reads the list
/// every POLL.
pub struct Approval {
    store: PinStore,
    kvm: Fingerprint,
    interval: Duration,
    next_check: Cell<Instant>,
    withdrawn: Cell<bool>,
}

impl Approval {
    /// The approval of `kvm`, which the handshake found in `store`.
    pub fn of(store: PinStore, kvm: Fingerprint) -> Self {
        Self::read_every(POLL, store, kvm)
    }

    /// An approval that reads the list every `interval`; a test picks an
    /// interval shorter than POLL.
    pub fn read_every(interval: Duration, store: PinStore, kvm: Fingerprint) -> Self {
        Self {
            store,
            kvm,
            interval,
            next_check: Cell::new(Instant::now() + interval),
            withdrawn: Cell::new(false),
        }
    }

    pub fn kvm(&self) -> Fingerprint {
        self.kvm
    }

    /// Whether the KVM is no longer in the approved list. A list that
    /// cannot be read approves nothing, as it does when a session opens.
    pub fn withdrawn(&self) -> bool {
        if self.withdrawn.get() || Instant::now() < self.next_check.get() {
            return self.withdrawn.get();
        }
        self.next_check.set(Instant::now() + self.interval);
        let kvm = &self.kvm;
        match self.store.load() {
            Ok(approved) if approved.contains(kvm) => return false,
            Ok(_) => eprintln!(
                "ipkvm-os-services: the KVM {kvm} is no longer approved; closing its session"
            ),
            Err(error) => eprintln!(
                "ipkvm-os-services: could not read the approved KVMs ({error}); closing the \
                 session with the KVM {kvm}"
            ),
        }
        self.withdrawn.set(true);
        true
    }
}

/// Asks the user to approve `kvm` and waits. Returns when the user approves
/// it (in the dialog or with `--approve-kvm`), when a KVM is attached,
/// detached or replaced, or when the process is asked to stop. A denied
/// dialog stays denied until then.
pub fn wait_for_approval(kvm: &Fingerprint, shutdown: &AtomicBool) -> Result<(), Box<dyn Error>> {
    eprintln!(
        "ipkvm-os-services: the KVM {kvm} is not approved; select Allow in the \
         dialog, or run: ipkvm-os-services --approve-kvm {kvm}"
    );
    let store = PinStore::for_user()?;
    let mut dialog = Dialog::open(kvm, !store.load()?.is_empty());
    let attached = protocol::attached_interfaces();
    while !shutdown.load(Ordering::Relaxed) && protocol::attached_interfaces() == attached {
        if dialog.approved()? {
            store.approve(kvm)?;
        }
        if store.load()?.contains(kvm) {
            eprintln!("ipkvm-os-services: approved the KVM {kvm}");
            break;
        }
        thread::sleep(POLL);
    }
    Ok(())
}

/// A dialog, a separate process so that it closes when the KVM disconnects.
pub struct Dialog(Option<Child>);

impl Dialog {
    fn open(kvm: &Fingerprint, replaces_another: bool) -> Self {
        let text = dialog_text(kvm, replaces_another);
        let dialog = Self::spawn(dialog_commands(&text));
        if !dialog.is_open() {
            eprintln!("ipkvm-os-services: no dialog program found to ask for approval");
        }
        dialog
    }

    /// Starts the first of `commands` that runs. Each one exits with success
    /// only when the user accepts.
    pub fn spawn(commands: Vec<Command>) -> Self {
        let child = commands.into_iter().find_map(|mut command| {
            let command = command.stdout(Stdio::null()).stderr(Stdio::null());
            command.spawn().ok()
        });
        Self(child)
    }

    /// Whether the dialog is on screen: started, and not answered yet.
    pub fn is_open(&self) -> bool {
        self.0.is_some()
    }

    /// Whether the user answered with approve. A dialog that is still open,
    /// or that the user denied or closed, is not an approval.
    pub fn approved(&mut self) -> io::Result<bool> {
        let Some(child) = &mut self.0 else {
            return Ok(false);
        };
        let answer = child.try_wait()?;
        if answer.is_some() {
            self.0 = None;
        }
        Ok(answer.is_some_and(|status| status.success()))
    }
}

impl Drop for Dialog {
    fn drop(&mut self) {
        if let Some(child) = &mut self.0 {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// The dialog's title is its question, so the Yes and No buttons of the
/// Windows message box answer it.
pub const DIALOG_TITLE: &str = "Allow this KVM?";

pub fn dialog_text(kvm: &Fingerprint, replaces_another: bool) -> String {
    let warning = if replaces_another {
        "This is not the KVM you allowed before.\n\n"
    } else {
        ""
    };
    format!(
        "{warning}It can see your screen, use your clipboard, open a terminal and \
         save files.\n\n\
         Fingerprint: {kvm}\n\
         Allow only if your KVM's settings show this fingerprint."
    )
}

/// The dialog programs to try, in order. Each one exits with success only
/// when the user approves.
fn dialog_commands(text: &str) -> Vec<Command> {
    #[cfg(target_os = "linux")]
    {
        // zenity shows the text as plain text, not as Pango markup.
        let mut zenity = Command::new("zenity");
        zenity.args(["--question", "--no-markup", "--default-cancel"]);
        zenity.args(["--title", DIALOG_TITLE, "--text", text]);
        zenity.args(["--ok-label", "Allow", "--cancel-label", "Don't Allow"]);
        vec![zenity]
    }
    #[cfg(target_os = "macos")]
    {
        let mut osascript = Command::new("osascript");
        osascript.args(["-e", "on run argv", "-e"]);
        osascript.arg(format!(
            "display dialog (item 1 of argv) with title \"{DIALOG_TITLE}\" with icon caution \
             buttons {{\"Don't Allow\", \"Allow\"}} default button \"Don't Allow\" \
             cancel button \"Don't Allow\""
        ));
        osascript.args(["-e", "end run", text]);
        vec![osascript]
    }
    #[cfg(windows)]
    {
        // The dialog is this program's own message box, so that the sender
        // can close it.
        let Ok(executable) = std::env::current_exe() else {
            return Vec::new();
        };
        let mut dialog = Command::new(executable);
        dialog.args(["--approval-dialog", text]);
        vec![dialog]
    }
}

/// Shows the approval message box; true when the user selects Yes.
#[cfg(windows)]
pub fn message_box(text: &str) -> bool {
    use std::{ffi::OsStr, os::windows::ffi::OsStrExt, ptr};
    use winapi::um::winuser::{
        MessageBoxW, IDYES, MB_DEFBUTTON2, MB_ICONWARNING, MB_SETFOREGROUND, MB_TOPMOST, MB_YESNO,
    };
    let wide = |text: &str| -> Vec<u16> { OsStr::new(text).encode_wide().chain([0]).collect() };
    let flags = MB_YESNO | MB_ICONWARNING | MB_DEFBUTTON2 | MB_TOPMOST | MB_SETFOREGROUND;
    let answer = unsafe {
        MessageBoxW(
            ptr::null_mut(),
            wide(text).as_ptr(),
            wide(DIALOG_TITLE).as_ptr(),
            flags,
        )
    };
    answer == IDYES
}

#[cfg(test)]
mod tests {
    use super::*;

    const KVM: &str = "34:75:0F:98:BD:59:FC:FC";
    const OTHER: &str = "00:75:0F:98:BD:59:FC:FC";

    fn store(name: &str) -> PinStore {
        PinStore::temporary(name)
    }

    #[test]
    fn approves_and_forgets_kvms() {
        let store = store("approve");
        let (kvm, other) = (
            Fingerprint::parse(KVM).unwrap(),
            Fingerprint::parse(OTHER).unwrap(),
        );
        assert_eq!(store.load().unwrap(), []);
        store.approve(&kvm).unwrap();
        store.approve(&other).unwrap();
        store.approve(&kvm).unwrap();
        assert_eq!(store.load().unwrap(), [kvm, other]);
        assert!(store.forget(&kvm).unwrap());
        assert!(!store.forget(&kvm).unwrap());
        assert_eq!(store.load().unwrap(), [other]);
    }

    /// A store whose file holds `damage`.
    fn damaged_store(name: &str, damage: &[u8]) -> PinStore {
        let store = store(name);
        fs::create_dir_all(store.path.parent().unwrap()).unwrap();
        fs::write(&store.path, damage).unwrap();
        store
    }

    #[test]
    fn a_damaged_store_approves_nothing() {
        let damage = format!("{KVM}\nnot a fingerprint\n");
        let store = damaged_store("damaged", damage.as_bytes());
        assert!(store.load().is_err());
    }

    #[test]
    fn approving_a_kvm_replaces_a_damaged_store() {
        let other = Fingerprint::parse(OTHER).unwrap();
        let damage = format!("{KVM}\nnot a fingerprint\n");
        for (name, damage) in [
            ("text", damage.as_bytes()),
            ("bytes", b"\xff\xfe".as_slice()),
        ] {
            let store = damaged_store(&format!("approve-{name}"), damage);
            store.approve(&other).unwrap();
            assert_eq!(store.load().unwrap(), [other], "{name}");
        }
    }

    #[test]
    fn forgetting_a_kvm_empties_a_damaged_store() {
        let kvm = Fingerprint::parse(KVM).unwrap();
        let damage = format!("{KVM}\nnot a fingerprint\n");
        let store = damaged_store("forget", damage.as_bytes());
        assert!(!store.forget(&kvm).unwrap());
        assert_eq!(store.load().unwrap(), []);
    }
}
