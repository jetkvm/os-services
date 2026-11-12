//! Updates from the KVM. The KVM chooses the release, checks its package, and
//! relays the package for this installation over USB with its size and
//! SHA-256. The package is written to a staging file under a credit window
//! and checked against that size and hash. The session then ends, and the
//! supervisor installs the package.

use super::credit::{Credit, WINDOW};
use crate::protocol::{UpdateRecord, UpdateReply, UpdateStatus};
use crate::supervisor::update::{self as installer, Installation, Staged};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::Write,
    path::PathBuf,
    sync::{Arc, Mutex},
};

/// The largest package the sender stages: far above any release, and small
/// enough that a wrong size cannot fill the disk.
const MAX_PACKAGE: u64 = 1 << 30;

/// The package being written.
struct Active {
    id: u32,
    size: u64,
    sha256: [u8; 32],
    hasher: Sha256,
    file: File,
    path: PathBuf,
    received: u64,
    credit: Credit,
}

/// The package that completed, for the session to install once it ends.
pub type StagedUpdate = Arc<Mutex<Option<Staged>>>;

/// One session's update reception, driven by the thread that reads the USB
/// link; replies go out through `reply`, on whatever thread owns the wire.
pub struct Updates {
    installation: Option<Installation>,
    active: Option<Active>,
    staged: StagedUpdate,
    reply: Box<dyn Fn(UpdateReply) + Send>,
}

impl Updates {
    pub fn new(
        installation: Option<Installation>,
        staged: StagedUpdate,
        reply: impl Fn(UpdateReply) + Send + 'static,
    ) -> Self {
        Self {
            installation,
            active: None,
            staged,
            reply: Box::new(reply),
        }
    }

    pub fn handle(&mut self, record: UpdateRecord) {
        match record {
            UpdateRecord::Begin {
                update_id,
                size,
                sha256,
                version,
            } => self.begin(update_id, size, sha256, &version),
            UpdateRecord::Chunk {
                update_id,
                offset,
                data,
            } => self.receive_chunk(update_id, offset, &data),
            UpdateRecord::Cancel { update_id } => {
                if self.active.as_ref().is_some_and(|a| a.id == update_id) {
                    self.finish(UpdateStatus::Cancelled, String::new());
                }
            }
        }
    }

    fn status(&self, update_id: u32, status: UpdateStatus, message: String) {
        (self.reply)(UpdateReply::Status {
            update_id,
            status,
            message,
        });
    }

    fn begin(&mut self, update_id: u32, size: u64, sha256: [u8; 32], version: &str) {
        // A new update replaces the open one.
        self.finish(UpdateStatus::Cancelled, String::new());
        let (file, path) = match self.create_staging_file(size) {
            Ok(created) => created,
            Err(message) => {
                eprintln!("ipkvm-os-services: update to {version} refused: {message}");
                return self.status(update_id, UpdateStatus::Failed, message);
            }
        };
        eprintln!("ipkvm-os-services: receiving the update to {version}");
        (self.reply)(UpdateReply::Ready {
            update_id,
            credit: WINDOW,
        });
        self.active = Some(Active {
            id: update_id,
            size,
            sha256,
            hasher: Sha256::new(),
            file,
            path,
            received: 0,
            credit: Credit::new(),
        });
    }

    fn create_staging_file(&self, size: u64) -> Result<(File, PathBuf), String> {
        if self.staged.lock().expect("staged update lock").is_some() {
            return Err("an update is already installing".into());
        }
        let installation = self
            .installation
            .as_ref()
            .ok_or("this installation cannot update itself")?;
        if size > MAX_PACKAGE {
            return Err(format!("the package is too large: {size} bytes"));
        }
        installation
            .create_staging_file()
            .map_err(|err| format!("could not create the staging file: {err}"))
    }

    fn receive_chunk(&mut self, update_id: u32, offset: u64, data: &[u8]) {
        // A chunk for an update that ended is the KVM draining its credit.
        let Some(active) = self.active.as_mut().filter(|a| a.id == update_id) else {
            return;
        };
        let end = active.received + data.len() as u64;
        if offset != active.received || !active.credit.covers(end) || end > active.size {
            return self.finish(
                UpdateStatus::Failed,
                "an update chunk is out of sequence or exceeds its credit".into(),
            );
        }
        if let Err(err) = active.file.write_all(data) {
            return self.finish(
                UpdateStatus::Failed,
                format!("writing the package failed: {err}"),
            );
        }
        active.hasher.update(data);
        active.received = end;
        if end == active.size {
            self.complete();
        } else if let Some(bytes) = active.credit.consume(data.len()) {
            (self.reply)(UpdateReply::Credit { update_id, bytes });
        }
    }

    /// Checks the complete package and hands it to the session. The status
    /// is queued before the session learns of the package, so it reaches the
    /// KVM ahead of session.close.
    fn complete(&mut self) {
        let Some(active) = self.active.take() else {
            return;
        };
        let synced = active.file.sync_all();
        drop(active.file);
        let digest: [u8; 32] = active.hasher.finalize().into();
        let installation = self.installation.as_ref().expect("an update is open");
        let checked = if digest != active.sha256 {
            Err("the package does not match its SHA-256".to_owned())
        } else {
            synced.map_err(|err| format!("writing the package failed: {err}"))
        }
        .and_then(|()| installation.check_publisher(&active.path));
        match checked {
            Ok(()) => {
                self.status(active.id, UpdateStatus::Installing, String::new());
                *self.staged.lock().expect("staged update lock") =
                    Some(installation.stage(active.path));
            }
            Err(message) => {
                eprintln!("ipkvm-os-services: update failed: {message}");
                installer::discard(&active.path);
                self.status(active.id, UpdateStatus::Failed, message);
            }
        }
    }

    /// Ends the open update without installing it, and deletes its staging
    /// file.
    fn finish(&mut self, status: UpdateStatus, message: String) {
        let Some(active) = self.active.take() else {
            return;
        };
        drop(active.file);
        installer::discard(&active.path);
        if !message.is_empty() {
            eprintln!("ipkvm-os-services: update failed: {message}");
        }
        self.status(active.id, status, message);
    }
}

impl Drop for Updates {
    /// The session ended: nothing more arrives, and nothing partial stays
    /// behind.
    fn drop(&mut self) {
        if let Some(active) = self.active.take() {
            drop(active.file);
            installer::discard(&active.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::credit::STEP;
    use std::fs;

    type Replies = Arc<Mutex<Vec<UpdateReply>>>;

    struct Harness {
        updates: Updates,
        replies: Replies,
        staged: StagedUpdate,
        dir: PathBuf,
    }

    fn harness(tag: &str) -> Harness {
        let dir = std::env::temp_dir().join(format!("update-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let replies: Replies = Arc::default();
        let sink = replies.clone();
        let staged = StagedUpdate::default();
        let updates = Updates::new(
            Some(Installation::for_test(dir.join("package"))),
            staged.clone(),
            move |reply| sink.lock().unwrap().push(reply),
        );
        Harness {
            updates,
            replies,
            staged,
            dir,
        }
    }

    impl Harness {
        fn take(&self) -> Vec<UpdateReply> {
            std::mem::take(&mut *self.replies.lock().unwrap())
        }

        fn begin(&mut self, update_id: u32, package: &[u8]) {
            self.updates.handle(UpdateRecord::Begin {
                update_id,
                size: package.len() as u64,
                sha256: Sha256::digest(package).into(),
                version: "9.9.9".into(),
            });
        }

        fn chunk(&mut self, update_id: u32, offset: u64, data: &[u8]) {
            self.updates.handle(UpdateRecord::Chunk {
                update_id,
                offset,
                data: data.to_vec(),
            });
        }

        fn staging_file(&self) -> PathBuf {
            self.dir.join("package")
        }
    }

    impl Drop for Harness {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    fn status(update_id: u32, status: UpdateStatus, message: &str) -> UpdateReply {
        UpdateReply::Status {
            update_id,
            status,
            message: message.into(),
        }
    }

    #[test]
    fn a_complete_package_with_its_hash_is_staged_for_install() {
        let mut h = harness("complete");
        h.begin(1, b"package");
        assert_eq!(
            h.take(),
            vec![UpdateReply::Ready {
                update_id: 1,
                credit: WINDOW
            }]
        );
        h.chunk(1, 0, b"pack");
        assert!(h.staged.lock().unwrap().is_none());
        h.chunk(1, 4, b"age");
        assert_eq!(h.take(), vec![status(1, UpdateStatus::Installing, "")]);
        assert!(h.staged.lock().unwrap().is_some());
        assert_eq!(fs::read(h.staging_file()).unwrap(), b"package");
    }

    #[test]
    fn a_package_with_the_wrong_hash_is_deleted() {
        let mut h = harness("hash");
        h.updates.handle(UpdateRecord::Begin {
            update_id: 1,
            size: 7,
            sha256: [0; 32],
            version: "9.9.9".into(),
        });
        h.take();
        h.chunk(1, 0, b"package");
        assert_eq!(
            h.take(),
            vec![status(
                1,
                UpdateStatus::Failed,
                "the package does not match its SHA-256"
            )]
        );
        assert!(h.staged.lock().unwrap().is_none());
        assert!(!h.staging_file().exists());
    }

    /// The update of a release AppImage: the installed image is signed by
    /// the test release key of the publisher fixtures.
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    fn release_harness(tag: &str) -> Harness {
        let mut h = harness(tag);
        let installed = h.dir.join("installed.AppImage");
        let image = include_bytes!("../supervisor/publisher/fixtures/own.elf");
        fs::write(&installed, image).unwrap();
        h.updates.installation =
            Some(Installation::for_test(h.staging_file()).with_target(installed));
        h
    }

    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    #[test]
    fn a_release_stages_a_package_of_its_publisher() {
        let mut h = release_harness("publisher");
        let package = include_bytes!("../supervisor/publisher/fixtures/signing-key-2.elf");
        h.begin(1, package);
        h.take();
        h.chunk(1, 0, package);
        assert_eq!(h.take(), vec![status(1, UpdateStatus::Installing, "")]);
        assert!(h.staged.lock().unwrap().is_some());
    }

    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    #[test]
    fn a_release_deletes_a_package_of_another_publisher() {
        let mut h = release_harness("other-publisher");
        let package = include_bytes!("../supervisor/publisher/fixtures/other-publisher.elf");
        h.begin(1, package);
        h.take();
        h.chunk(1, 0, package);
        let replies = h.take();
        assert!(
            matches!(
                &replies[..],
                [UpdateReply::Status {
                    update_id: 1,
                    status: UpdateStatus::Failed,
                    message,
                }] if message == "the package is refused: another publisher signed it"
            ),
            "{replies:?}"
        );
        assert!(h.staged.lock().unwrap().is_none());
        assert!(!h.staging_file().exists());
    }

    #[test]
    fn a_chunk_out_of_sequence_or_beyond_credit_fails_the_update() {
        let mut h = harness("sequence");
        h.begin(1, b"package");
        h.take();
        h.chunk(1, 1, b"ackage");
        assert!(matches!(
            h.take()[..],
            [UpdateReply::Status {
                update_id: 1,
                status: UpdateStatus::Failed,
                ..
            }]
        ));
        assert!(!h.staging_file().exists());

        let package = vec![7u8; WINDOW as usize + 1];
        h.begin(2, &package);
        h.take();
        h.chunk(2, 0, &package);
        assert!(matches!(
            h.take()[..],
            [UpdateReply::Status {
                update_id: 2,
                status: UpdateStatus::Failed,
                ..
            }]
        ));
    }

    #[test]
    fn credit_follows_consumption() {
        let mut h = harness("credit");
        h.begin(1, &vec![0u8; WINDOW as usize * 2]);
        h.take();
        h.chunk(1, 0, &vec![0u8; STEP as usize]);
        assert_eq!(
            h.take(),
            vec![UpdateReply::Credit {
                update_id: 1,
                bytes: STEP as u32
            }]
        );
    }

    #[test]
    fn a_new_begin_or_a_cancel_ends_the_open_update() {
        let mut h = harness("replace");
        h.begin(1, b"package");
        h.chunk(1, 0, b"pack");
        h.take();
        h.begin(2, b"package");
        assert_eq!(
            h.take(),
            vec![
                status(1, UpdateStatus::Cancelled, ""),
                UpdateReply::Ready {
                    update_id: 2,
                    credit: WINDOW
                }
            ]
        );
        // A chunk of the replaced update is ignored.
        h.chunk(1, 4, b"age");
        assert!(h.take().is_empty());
        h.updates.handle(UpdateRecord::Cancel { update_id: 2 });
        assert_eq!(h.take(), vec![status(2, UpdateStatus::Cancelled, "")]);
        assert!(!h.staging_file().exists());
    }

    #[test]
    fn an_installation_that_cannot_update_refuses_the_package() {
        let replies: Replies = Arc::default();
        let sink = replies.clone();
        let mut updates = Updates::new(None, StagedUpdate::default(), move |reply| {
            sink.lock().unwrap().push(reply)
        });
        updates.handle(UpdateRecord::Begin {
            update_id: 1,
            size: 7,
            sha256: [0; 32],
            version: "9.9.9".into(),
        });
        assert_eq!(
            *replies.lock().unwrap(),
            vec![status(
                1,
                UpdateStatus::Failed,
                "this installation cannot update itself"
            )]
        );
    }

    #[test]
    fn a_staged_update_refuses_another() {
        let mut h = harness("staged");
        h.begin(1, b"package");
        h.chunk(1, 0, b"package");
        h.take();
        h.begin(2, b"package");
        assert_eq!(
            h.take(),
            vec![status(
                2,
                UpdateStatus::Failed,
                "an update is already installing"
            )]
        );
    }

    #[test]
    fn dropping_the_session_removes_the_partial_package() {
        let mut h = harness("session");
        h.begin(1, b"package");
        h.chunk(1, 0, b"pack");
        assert!(h.staging_file().exists());
        let path = h.staging_file();
        drop(std::mem::replace(
            &mut h.updates,
            Updates::new(None, StagedUpdate::default(), |_| {}),
        ));
        assert!(!path.exists());
    }
}
