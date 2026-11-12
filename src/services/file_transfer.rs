//! File reception. The browser drops files on the KVM, which relays them
//! over USB; they are written one at a time into the user's Downloads
//! folder, or the home folder when there is none. A credit window paces
//! the transfer, and a partial file is deleted.

use super::credit::{Credit, WINDOW};
use crate::protocol::{DroppedFile, FileCapabilities, FileRecord, FileReply, FileStatus};
use std::{
    collections::VecDeque,
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Component, Path, PathBuf},
};

pub fn capabilities(allowed: bool) -> FileCapabilities {
    FileCapabilities {
        receive: allowed && receive_dir().is_some(),
    }
}

/// The file being written. Files are written one at a time, in drop order.
struct Active {
    id: u32,
    name: String,
    size: u64,
    file: File,
    path: PathBuf,
    received: u64,
    credit: Credit,
}

/// One session's file reception, driven by the thread that reads the USB
/// link; replies go out through `reply`, on whatever thread owns the wire.
pub struct Files {
    destination: Option<PathBuf>,
    allowed: bool,
    pending: VecDeque<DroppedFile>,
    active: Option<Active>,
    reply: Box<dyn Fn(FileReply) + Send>,
}

impl Files {
    pub fn new(allowed: bool, reply: impl Fn(FileReply) + Send + 'static) -> Self {
        let mut files = Self::in_dir(receive_dir(), reply);
        files.allowed = allowed;
        files
    }

    fn in_dir(destination: Option<PathBuf>, reply: impl Fn(FileReply) + Send + 'static) -> Self {
        Self {
            destination,
            allowed: true,
            pending: VecDeque::new(),
            active: None,
            reply: Box::new(reply),
        }
    }

    pub fn set_allowed(&mut self, allowed: bool) {
        self.allowed = allowed;
        if !allowed {
            // Drain queued transfers first: finish must not open the next file.
            while let Some(file) = self.pending.pop_front() {
                self.done(file.transfer_id, FileStatus::Cancelled, Path::new(""));
            }
            self.finish(FileStatus::Cancelled);
        }
    }

    pub fn handle(&mut self, record: FileRecord) {
        match record {
            FileRecord::Drop { files } => {
                if !self.allowed {
                    for file in files {
                        self.done(file.transfer_id, FileStatus::Failed, Path::new(""));
                    }
                    return;
                }
                self.pending.extend(files);
                self.open_next();
            }
            FileRecord::Chunk {
                transfer_id,
                offset,
                data,
            } => self.receive_chunk(transfer_id, offset, &data),
            FileRecord::Cancel { transfer_id } => self.cancel(transfer_id),
        }
    }

    fn done(&self, transfer_id: u32, status: FileStatus, path: &Path) {
        let path = match status {
            FileStatus::Ok => path.to_string_lossy().into_owned(),
            _ => String::new(),
        };
        (self.reply)(FileReply::Done {
            transfer_id,
            status,
            path,
        });
    }

    /// Creates the next queued file once no file is being written.
    fn open_next(&mut self) {
        while self.active.is_none() {
            let Some(next) = self.pending.pop_front() else {
                return;
            };
            let created = self
                .destination
                .as_deref()
                .ok_or_else(|| "no Downloads or home folder is available".to_owned())
                .and_then(|dir| create_unique(dir, &next.name));
            let (file, path) = match created {
                Ok(created) => created,
                Err(err) => {
                    eprintln!("ipkvm-os-services: could not create {}: {err}", next.name);
                    self.done(next.transfer_id, FileStatus::Failed, Path::new(""));
                    continue;
                }
            };
            mark_as_received(&path);
            (self.reply)(FileReply::Ready {
                transfer_id: next.transfer_id,
                credit: WINDOW,
            });
            if next.size == 0 {
                // Nothing will arrive; the file is complete as created.
                self.done(next.transfer_id, FileStatus::Ok, &path);
                continue;
            }
            self.active = Some(Active {
                id: next.transfer_id,
                name: next.name,
                size: next.size,
                file,
                path,
                received: 0,
                credit: Credit::new(),
            });
        }
    }

    fn receive_chunk(&mut self, transfer_id: u32, offset: u64, data: &[u8]) {
        // A chunk for a transfer that ended is the KVM draining its credit.
        let Some(active) = self.active.as_mut().filter(|a| a.id == transfer_id) else {
            return;
        };
        let end = active.received + data.len() as u64;
        if offset != active.received || !active.credit.covers(end) || end > active.size {
            eprintln!(
                "ipkvm-os-services: file chunk for {} is out of sequence or exceeds credit",
                active.name
            );
            return self.finish(FileStatus::Failed);
        }
        if let Err(err) = active.file.write_all(data) {
            eprintln!("ipkvm-os-services: writing {} failed: {err}", active.name);
            return self.finish(FileStatus::Failed);
        }
        active.received = end;
        if end == active.size {
            self.finish(FileStatus::Ok);
        } else if let Some(bytes) = active.credit.consume(data.len()) {
            (self.reply)(FileReply::Credit { transfer_id, bytes });
        }
    }

    fn cancel(&mut self, transfer_id: u32) {
        if self.active.as_ref().is_some_and(|a| a.id == transfer_id) {
            self.finish(FileStatus::Cancelled);
        } else if let Some(at) = self
            .pending
            .iter()
            .position(|f| f.transfer_id == transfer_id)
        {
            self.pending.remove(at);
            self.done(transfer_id, FileStatus::Cancelled, Path::new(""));
        }
    }

    /// Ends the active transfer: keeps a complete file, deletes a partial
    /// one, reports it, and starts the next file in the queue.
    fn finish(&mut self, status: FileStatus) {
        let Some(active) = self.active.take() else {
            return;
        };
        drop(active.file);
        if status != FileStatus::Ok {
            let _ = fs::remove_file(&active.path);
        }
        self.done(active.id, status, &active.path);
        self.open_next();
    }
}

impl Drop for Files {
    /// The session ended: nothing more arrives, and nothing partial stays
    /// behind.
    fn drop(&mut self) {
        if let Some(active) = self.active.take() {
            drop(active.file);
            let _ = fs::remove_file(active.path);
        }
    }
}

fn receive_dir() -> Option<PathBuf> {
    // Respect redirected Windows known folders and Linux XDG user directories.
    existing_destination(dirs::download_dir(), dirs::home_dir())
}

fn existing_destination(downloads: Option<PathBuf>, home: Option<PathBuf>) -> Option<PathBuf> {
    downloads
        .filter(|path| path.is_dir())
        .or_else(|| home.filter(|path| path.is_dir()))
}

/// Marks the file as one that came from another computer, as a browser
/// marks a download, so that the OS checks it before it opens or runs it.
/// Windows and macOS have such a mark. A file system that cannot store the
/// mark leaves the file without it.
#[cfg(windows)]
fn mark_as_received(path: &Path) {
    let mut stream = path.as_os_str().to_owned();
    stream.push(":Zone.Identifier");
    // Zone 3 is the Internet zone.
    let _ = fs::write(stream, "[ZoneTransfer]\r\nZoneId=3\r\n");
}

#[cfg(target_os = "macos")]
fn mark_as_received(path: &Path) {
    use std::{
        os::unix::ffi::OsStrExt,
        time::{SystemTime, UNIX_EPOCH},
    };
    let Ok(path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return;
    };
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());
    // The flags of a download that the user has not opened yet, the time,
    // and the program that stored the file.
    let quarantine = format!("0081;{seconds:08x};IP-KVM OS Services;");
    unsafe {
        libc::setxattr(
            path.as_ptr(),
            c"com.apple.quarantine".as_ptr(),
            quarantine.as_ptr().cast(),
            quarantine.len(),
            0,
            0,
        );
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
fn mark_as_received(_: &Path) {}

/// Whether this computer stores `name` as a file with exactly that name in
/// the destination folder. The protocol makes the name one path component;
/// this refuses what the OS or a file manager reads differently.
fn storable(name: &str) -> bool {
    let mut components = Path::new(name).components();
    // Embeddings, overrides and isolates make a file manager show the name
    // in another order than it is stored, which can hide the extension.
    let reorders = |c| matches!(c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}');
    matches!(
        (components.next(), components.next()),
        (Some(Component::Normal(_)), None)
    ) && !name.chars().any(reorders)
        && (!cfg!(windows) || storable_on_windows(name))
}

/// Windows reads a colon as a drive or as a stream of another file, drops
/// trailing dots and spaces, and opens a device for a reserved name, with
/// or without an extension.
fn storable_on_windows(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or(name).trim_end_matches(' ');
    let stem = stem.to_ascii_uppercase();
    let port = |prefix| {
        stem.strip_prefix(prefix)
            .is_some_and(|number| number.chars().count() == 1 && "0123456789¹²³".contains(number))
    };
    let device = matches!(
        stem.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) || port("COM")
        || port("LPT");
    !name.contains(':') && !name.ends_with(['.', ' ']) && !device
}

/// Creates `name` in `dir`, or the first "name (n)" that does not exist yet.
fn create_unique(dir: &Path, name: &str) -> Result<(File, PathBuf), String> {
    if !storable(name) {
        return Err("this computer cannot store a file with that name".into());
    }
    let (stem, extension) = match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => (stem, Some(ext)),
        _ => (name, None),
    };
    for attempt in 0..1000u32 {
        let candidate = match (attempt, extension) {
            (0, _) => name.to_owned(),
            (n, Some(ext)) => format!("{stem} ({}).{ext}", n + 1),
            (n, None) => format!("{stem} ({})", n + 1),
        };
        let path = dir.join(candidate);
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => return Ok((file, path)),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(format!("{}: {err}", path.display())),
        }
    }
    Err(format!("too many files named {name}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::credit::STEP as CREDIT_STEP;
    use std::sync::{Arc, Mutex};

    type Replies = Arc<Mutex<Vec<FileReply>>>;

    fn files_in_temp_dir(tag: &str) -> (Files, Replies, PathBuf) {
        let dir = std::env::temp_dir().join(format!("file-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let replies: Replies = Arc::default();
        let sink = replies.clone();
        let files = Files::in_dir(Some(dir.clone()), move |reply| {
            sink.lock().unwrap().push(reply)
        });
        (files, replies, dir)
    }

    fn take(replies: &Replies) -> Vec<FileReply> {
        std::mem::take(&mut *replies.lock().unwrap())
    }

    fn dropped(transfer_id: u32, size: u64, name: &str) -> DroppedFile {
        DroppedFile {
            transfer_id,
            size,
            name: name.into(),
        }
    }

    fn chunk(transfer_id: u32, offset: u64, data: &[u8]) -> FileRecord {
        FileRecord::Chunk {
            transfer_id,
            offset,
            data: data.to_vec(),
        }
    }

    #[test]
    fn downloads_are_preferred_with_home_as_the_fallback() {
        let (_, _, home) = files_in_temp_dir("destination");
        let downloads = home.join("custom-downloads");
        assert_eq!(
            existing_destination(None, Some(home.clone())),
            Some(home.clone())
        );
        assert_eq!(
            existing_destination(Some(downloads.clone()), Some(home.clone())),
            Some(home.clone())
        );
        fs::write(&downloads, b"not a directory").unwrap();
        assert_eq!(
            existing_destination(Some(downloads.clone()), Some(home.clone())),
            Some(home.clone())
        );
        fs::remove_file(&downloads).unwrap();
        fs::create_dir(&downloads).unwrap();
        assert_eq!(
            existing_destination(Some(downloads.clone()), Some(home.clone())),
            Some(downloads.clone())
        );
        assert_eq!(
            existing_destination(Some(downloads), None),
            Some(home.join("custom-downloads"))
        );
        assert_eq!(existing_destination(None, None), None);
        assert_eq!(existing_destination(None, Some(home.join("missing"))), None);
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn missing_destination_refuses_the_transfer() {
        let replies: Replies = Arc::default();
        let sink = replies.clone();
        let mut files = Files::in_dir(None, move |reply| sink.lock().unwrap().push(reply));
        files.handle(FileRecord::Drop {
            files: vec![dropped(1, 0, "no-destination.txt")],
        });
        assert_eq!(
            take(&replies),
            vec![FileReply::Done {
                transfer_id: 1,
                status: FileStatus::Failed,
                path: String::new(),
            }]
        );
    }

    #[test]
    fn disabling_files_cancels_active_and_queued_transfers_and_can_resume() {
        let (mut files, replies, dir) = files_in_temp_dir("permission");
        files.handle(FileRecord::Drop {
            files: vec![dropped(1, 8, "active.bin"), dropped(2, 8, "queued.bin")],
        });
        files.handle(chunk(1, 0, b"data"));
        take(&replies);
        files.set_allowed(false);
        assert!(!dir.join("active.bin").exists());
        assert!(!dir.join("queued.bin").exists());
        let cancelled = take(&replies);
        assert_eq!(cancelled.len(), 2);
        assert!(cancelled.iter().all(|r| matches!(
            r,
            FileReply::Done {
                status: FileStatus::Cancelled,
                ..
            }
        )));
        files.handle(chunk(1, 4, b"data"));
        files.handle(FileRecord::Drop {
            files: vec![dropped(3, 0, "blocked.txt")],
        });
        assert!(!dir.join("blocked.txt").exists());
        assert!(matches!(
            take(&replies)[0],
            FileReply::Done {
                status: FileStatus::Failed,
                ..
            }
        ));
        files.set_allowed(true);
        files.handle(FileRecord::Drop {
            files: vec![dropped(4, 0, "allowed.txt")],
        });
        assert!(dir.join("allowed.txt").exists());
        drop(files);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn unique_names_step_past_existing_files() {
        let (_, _, dir) = files_in_temp_dir("unique");
        let (_, first) = create_unique(&dir, "report.pdf").unwrap();
        let (_, second) = create_unique(&dir, "report.pdf").unwrap();
        let (_, third) = create_unique(&dir, "report.pdf").unwrap();
        assert_eq!(first.file_name().unwrap(), "report.pdf");
        assert_eq!(second.file_name().unwrap(), "report (2).pdf");
        assert_eq!(third.file_name().unwrap(), "report (3).pdf");
        let (_, dotfile) = create_unique(&dir, ".env").unwrap();
        let (_, dotfile2) = create_unique(&dir, ".env").unwrap();
        assert_eq!(dotfile.file_name().unwrap(), ".env");
        assert_eq!(dotfile2.file_name().unwrap(), ".env (2)");
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(windows)]
    #[test]
    fn a_received_file_has_the_mark_of_a_download() {
        let (mut files, _, dir) = files_in_temp_dir("mark");
        files.handle(FileRecord::Drop {
            files: vec![dropped(1, 4, "tool.exe")],
        });
        files.handle(chunk(1, 0, b"data"));
        assert_eq!(fs::read(dir.join("tool.exe")).unwrap(), b"data");
        let zone = fs::read_to_string(dir.join("tool.exe:Zone.Identifier")).unwrap();
        assert_eq!(zone, "[ZoneTransfer]\r\nZoneId=3\r\n");
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_received_file_is_in_quarantine() {
        use std::os::unix::ffi::OsStrExt;
        let (mut files, _, dir) = files_in_temp_dir("mark");
        files.handle(FileRecord::Drop {
            files: vec![dropped(1, 4, "tool.command")],
        });
        files.handle(chunk(1, 0, b"data"));
        let path = std::ffi::CString::new(dir.join("tool.command").as_os_str().as_bytes()).unwrap();
        let mut value = [0u8; 128];
        let length = unsafe {
            libc::getxattr(
                path.as_ptr(),
                c"com.apple.quarantine".as_ptr(),
                value.as_mut_ptr().cast(),
                value.len(),
                0,
                0,
            )
        };
        assert!(length > 0, "no quarantine attribute");
        let value = std::str::from_utf8(&value[..length as usize]).unwrap();
        assert!(value.starts_with("0081;"), "{value}");
        assert!(value.ends_with(";IP-KVM OS Services;"), "{value}");
        let _ = fs::remove_dir_all(&dir);
    }

    /// Drops one empty file and returns whether the transfer ended Ok.
    fn drop_stores(files: &mut Files, replies: &Replies, name: &str) -> bool {
        files.handle(FileRecord::Drop {
            files: vec![dropped(1, 0, name)],
        });
        take(replies).iter().any(|reply| {
            matches!(
                reply,
                FileReply::Done {
                    status: FileStatus::Ok,
                    ..
                }
            )
        })
    }

    #[test]
    fn a_name_that_shows_in_another_order_is_refused() {
        let (mut files, replies, dir) = files_in_temp_dir("reorder");
        assert!(!drop_stores(&mut files, &replies, "invoice\u{202e}fdp.exe"));
        assert!(!drop_stores(&mut files, &replies, "a\u{2066}b.txt"));
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);
        // A joiner inside an emoji is part of an ordinary name.
        assert!(drop_stores(
            &mut files,
            &replies,
            "family \u{1f468}\u{200d}\u{1f469}.txt"
        ));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn windows_refuses_drives_streams_devices_and_trailing_dots() {
        for name in [
            "C:escape.txt",
            "victim.txt:hidden",
            "victim.txt::$DATA",
            "NUL",
            "nul.txt",
            "Con",
            "AUX.tar.gz",
            "PRN ",
            "COM1",
            "com9.log",
            "LPT\u{b2}",
            "CONOUT$",
            "trail.txt.",
            "trail.txt ",
        ] {
            assert!(!storable_on_windows(name), "{name:?} accepted");
        }
        for name in [
            "report.pdf",
            ".hidden",
            "a b.txt",
            "console.txt",
            "COM10",
            "nullable",
            "auxiliary.md",
        ] {
            assert!(storable_on_windows(name), "{name:?} refused");
        }
    }

    /// The names go through the service on a real Windows file system: none
    /// of them leaves the folder, adds a stream to a file in it, or changes
    /// on the way to the disk.
    #[cfg(windows)]
    #[test]
    fn windows_writes_nothing_for_a_name_it_reads_differently() {
        let (mut files, replies, dir) = files_in_temp_dir("windows-names");
        fs::write(dir.join("victim.txt"), b"original").unwrap();
        // A drive-relative name resolves against the current directory of
        // that drive.
        let drive = dir.to_string_lossy()[..2].to_owned();
        let escape = format!("os-services-escape-{}.txt", std::process::id());
        let escaped = std::env::current_dir().unwrap().join(&escape);
        for name in [
            format!("{drive}{escape}"),
            "victim.txt:hidden".into(),
            "NUL".into(),
            "trail.txt.".into(),
            "trail.txt ".into(),
        ] {
            assert!(!drop_stores(&mut files, &replies, &name), "{name:?} stored");
        }
        let left_the_folder = fs::remove_file(&escaped).is_ok();
        assert!(!left_the_folder, "{escaped:?} was written");
        let names: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, ["victim.txt"]);
        assert!(fs::read(dir.join("victim.txt:hidden")).is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn files_are_written_one_at_a_time_in_drop_order() {
        let (mut files, replies, dir) = files_in_temp_dir("queue");
        files.handle(FileRecord::Drop {
            files: vec![
                dropped(1, 4, "first.bin"),
                dropped(2, 0, "empty.bin"),
                dropped(3, 4, "third.bin"),
            ],
        });
        // The first file is open and offered credit; the empty one is
        // complete as created; the third waits its turn.
        assert_eq!(
            take(&replies),
            vec![FileReply::Ready {
                transfer_id: 1,
                credit: WINDOW
            }]
        );
        assert_eq!(files.active.as_ref().unwrap().id, 1);

        files.handle(chunk(1, 0, b"data"));
        let after_first = take(&replies);
        assert_eq!(after_first.len(), 4, "{after_first:?}");
        assert!(matches!(
            after_first[0],
            FileReply::Done {
                transfer_id: 1,
                status: FileStatus::Ok,
                ..
            }
        ));
        assert!(matches!(
            after_first[1],
            FileReply::Ready { transfer_id: 2, .. }
        ));
        assert!(matches!(
            after_first[2],
            FileReply::Done {
                transfer_id: 2,
                status: FileStatus::Ok,
                ..
            }
        ));
        assert!(matches!(
            after_first[3],
            FileReply::Ready { transfer_id: 3, .. }
        ));
        assert_eq!(fs::read(dir.join("first.bin")).unwrap(), b"data");
        assert!(dir.join("empty.bin").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn credit_follows_consumption() {
        let (mut files, replies, dir) = files_in_temp_dir("credit");
        files.handle(FileRecord::Drop {
            files: vec![dropped(5, u64::from(WINDOW) * 2, "big.bin")],
        });
        take(&replies);
        let half_step = vec![0u8; CREDIT_STEP as usize / 2];
        // Consuming less than a step grants nothing; reaching it grants
        // exactly what was consumed.
        files.handle(chunk(5, 0, &half_step));
        assert!(take(&replies).is_empty());
        files.handle(chunk(5, CREDIT_STEP / 2, &half_step));
        assert_eq!(
            take(&replies),
            vec![FileReply::Credit {
                transfer_id: 5,
                bytes: CREDIT_STEP as u32
            }]
        );
        assert_eq!(
            files.active.as_ref().unwrap().credit.granted(),
            u64::from(WINDOW) + CREDIT_STEP
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_cancelled_or_misordered_transfer_leaves_no_partial_file() {
        let (mut files, replies, dir) = files_in_temp_dir("cancel");
        files.handle(FileRecord::Drop {
            files: vec![
                dropped(1, 8, "a.bin"),
                dropped(2, 8, "b.bin"),
                dropped(3, 8, "c.bin"),
            ],
        });
        files.handle(chunk(1, 0, b"data"));
        take(&replies);
        // Cancelling the active transfer deletes its partial file and opens
        // the next; cancelling a queued transfer only reports it.
        files.handle(FileRecord::Cancel { transfer_id: 1 });
        assert!(!dir.join("a.bin").exists());
        assert!(dir.join("b.bin").exists());
        files.handle(FileRecord::Cancel { transfer_id: 3 });
        let ended = take(&replies);
        assert!(matches!(
            ended[0],
            FileReply::Done {
                transfer_id: 1,
                status: FileStatus::Cancelled,
                ..
            }
        ));
        assert!(matches!(ended[1], FileReply::Ready { transfer_id: 2, .. }));
        assert!(matches!(
            ended[2],
            FileReply::Done {
                transfer_id: 3,
                status: FileStatus::Cancelled,
                ..
            }
        ));
        // A chunk at the wrong offset fails the transfer.
        files.handle(chunk(2, 4, b"data"));
        assert!(matches!(
            take(&replies)[0],
            FileReply::Done {
                transfer_id: 2,
                status: FileStatus::Failed,
                ..
            }
        ));
        assert!(!dir.join("b.bin").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn dropping_the_session_removes_the_partial_file() {
        let (mut files, _, dir) = files_in_temp_dir("session");
        files.handle(FileRecord::Drop {
            files: vec![dropped(1, 8, "a.bin")],
        });
        assert!(dir.join("a.bin").exists());
        drop(files);
        assert!(!dir.join("a.bin").exists());
        let _ = fs::remove_dir_all(&dir);
    }
}
