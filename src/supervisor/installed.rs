//! The installed program on Linux. An AppImage mounts its files with FUSE
//! each time it starts, and it runs from wherever the user keeps the file.
//! The service does not run that way: the files of the AppImage are copied
//! to `app` in the user's data folder, `~/.local/share/ipkvm-os-services`,
//! and the unit, the autostart entry and the applications menu entry start
//! the program there. Opening an AppImage makes the copy, an AppImage that
//! the unit starts moves itself there, and an update from the KVM replaces
//! the copy. The installed program needs no FUSE, and the AppImage file can
//! go.

use super::{publisher, update::start};
use std::{
    error::Error,
    fs,
    io::{self, Read},
    os::unix::fs::{symlink, PermissionsExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

const FOLDER: &str = "ipkvm-os-services";
/// The files of the installed AppImage, inside the folder.
const APP: &str = "app";
/// The link at the top of an AppImage's files that names its program.
const APP_RUN: &str = "AppRun";
/// The program's place below the top of an AppImage's files.
const PROGRAM_DIR: &str = "usr/bin";
/// The key block of the release key, when a release signed the installed
/// files. An update must carry a signature from this key (see `publisher`).
const PUBLISHER: &str = "publisher.asc";
const ICON: &str = "usr/share/icons/hicolor/512x512/apps/ipkvm-os-services.png";

/// The folder of the installation: `~/.local/share/ipkvm-os-services`.
pub fn folder() -> Option<PathBuf> {
    Some(dirs::data_dir()?.join(FOLDER))
}

/// The program that the unit and the menu entry start.
pub fn program(folder: &Path) -> PathBuf {
    folder.join(APP).join(APP_RUN)
}

/// The icon of the menu entry.
pub fn icon(folder: &Path) -> PathBuf {
    folder.join(APP).join(ICON)
}

/// Where an update is written before it is installed.
pub fn staging_path(folder: &Path) -> PathBuf {
    folder.join("update.AppImage")
}

/// The folder, when this program runs from one. The program's own place
/// tells: the unit runs without the variables of the session that
/// installed, so its data folder can be another one.
pub fn current() -> Option<PathBuf> {
    installation_of(&files()?)
}

/// The folder that these AppImage files are the app of, if they are in one.
fn installation_of(files: &Path) -> Option<PathBuf> {
    let folder = files.parent()?;
    let installed = files.file_name()? == APP && folder.file_name()? == FOLDER;
    installed.then(|| folder.to_path_buf())
}

/// The top of the AppImage files that this program is part of. A plain
/// cargo build is part of none.
fn files() -> Option<PathBuf> {
    let program = std::env::current_exe().ok()?;
    let top = program.parent()?.parent()?.parent()?;
    let part = program.parent()?.ends_with(PROGRAM_DIR) && top.join(APP_RUN).exists();
    part.then(|| top.to_path_buf())
}

/// The key block of the installed files, or None when no release signed
/// them. A key block that cannot be read is an error, not an installation
/// without a publisher: such an installation accepts every package.
pub fn publisher(folder: &Path) -> Result<Option<Vec<u8>>, String> {
    match fs::read(folder.join(APP).join(PUBLISHER)) {
        Ok(key_block) => Ok(Some(key_block)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!(
            "the publisher of this installation cannot be read: {error}"
        )),
    }
}

/// Copies the AppImage files of this program into the folder, and returns
/// the folder. A program that runs from the folder is installed already. A
/// plain cargo build has no such files, and the result is None.
pub fn install() -> Result<Option<PathBuf>, Box<dyn Error>> {
    let Some(files) = files() else {
        return Ok(None);
    };
    if let Some(folder) = installation_of(&files) {
        return Ok(Some(folder));
    }
    let folder = folder().ok_or("no user data directory")?;
    // The AppImage runtime names the image that the files come from. The
    // user opened that image, so its publisher is the installation's. Files
    // without an image keep the publisher that they carry.
    let publisher = match std::env::var_os("APPIMAGE") {
        Some(image) => Publisher::Set(publisher::adopt(None, Path::new(&image))?),
        None => Publisher::Carried,
    };
    install_files(&folder, &files, publisher)?;
    Ok(Some(folder))
}

/// The publisher of files that are installed.
enum Publisher {
    /// The key block that the installation keeps, or none for files that no
    /// release signed.
    Set(Option<Vec<u8>>),
    /// What the files state themselves.
    Carried,
}

/// Puts a copy of `files` in the folder as its app.
fn install_files(folder: &Path, files: &Path, publisher: Publisher) -> io::Result<()> {
    fs::create_dir_all(folder)?;
    let fresh = folder.join("app.new");
    remove(&fresh)?;
    let installed = copy_tree(files, &fresh)
        .and_then(|()| write_publisher(&fresh, publisher))
        .and_then(|()| replace(folder, &fresh));
    if installed.is_err() {
        let _ = remove(&fresh);
    }
    installed
}

fn write_publisher(app: &Path, publisher: Publisher) -> io::Result<()> {
    match publisher {
        Publisher::Set(Some(key_block)) => fs::write(app.join(PUBLISHER), key_block),
        Publisher::Set(None) => remove(&app.join(PUBLISHER)),
        Publisher::Carried => Ok(()),
    }
}

/// Replaces the installed files with the files of the AppImage `image`, and
/// removes the image. The new files get the key block that the installation
/// keeps (see `publisher`). An AppImage unpacks itself without FUSE.
pub fn install_image(folder: &Path, image: &Path) -> Result<(), Box<dyn Error>> {
    let kept = publisher::adopt(publisher(folder)?.as_deref(), image)?;
    let work = folder.join("update");
    remove(&work)?;
    fs::create_dir(&work)?;
    let installed = unpack(image, &work).and_then(|fresh| {
        write_publisher(&fresh, Publisher::Set(kept))?;
        Ok(replace(folder, &fresh)?)
    });
    let _ = remove(&work);
    installed?;
    let _ = fs::remove_file(image);
    Ok(())
}

/// Lets the image unpack itself into `work`, and returns its files once the
/// program in them is found runnable.
fn unpack(image: &Path, work: &Path) -> Result<PathBuf, Box<dyn Error>> {
    fs::set_permissions(image, fs::Permissions::from_mode(0o755))?;
    // The runtime of an AppImage writes its files to `squashfs-root` in the
    // current folder.
    let status = Command::new(image)
        .arg("--appimage-extract")
        .current_dir(work)
        .stdout(Stdio::null())
        .status()
        .map_err(|error| format!("could not unpack the package: {error}"))?;
    if !status.success() {
        return Err(format!("the package did not unpack: {status}").into());
    }
    let fresh = work.join("squashfs-root");
    let name = std::env::current_exe()?;
    let name = name.file_name().ok_or("the program has no name")?;
    check_runnable(&fresh.join(PROGRAM_DIR).join(name))?;
    start::check(&fresh.join(APP_RUN), start::WITHIN)?;
    Ok(fresh)
}

/// Fails unless the program is a file, not a link, that this user can
/// execute, and an x86-64 ELF program.
fn check_runnable(program: &Path) -> Result<(), Box<dyn Error>> {
    let metadata = fs::symlink_metadata(program)
        .map_err(|_| format!("the package holds no {}", program.display()))?;
    if !metadata.is_file() {
        return Err(format!("{} is not a file", program.display()).into());
    }
    if metadata.permissions().mode() & 0o100 == 0 {
        return Err(format!("{} is not executable", program.display()).into());
    }
    let mut head = Vec::new();
    fs::File::open(program)?.take(20).read_to_end(&mut head)?;
    if !x86_64_program(&head) {
        return Err(format!("{} is not an x86-64 program", program.display()).into());
    }
    Ok(())
}

/// Whether an ELF header is a 64-bit little-endian file for x86-64: the
/// magic number, the class, the byte order and the machine.
fn x86_64_program(header: &[u8]) -> bool {
    header.get(..6) == Some(b"\x7fELF\x02\x01".as_slice()) && header.get(18..20) == Some(&[62, 0])
}

/// Moves the installed files aside and the new files into their place, or
/// puts the installed files back. This program keeps running from the files
/// that are moved aside until it exits.
fn replace(folder: &Path, fresh: &Path) -> io::Result<()> {
    let app = folder.join(APP);
    let previous = folder.join("app.old");
    remove(&previous)?;
    let installed = fs::symlink_metadata(&app).is_ok();
    if installed {
        fs::rename(&app, &previous)?;
    }
    if let Err(error) = fs::rename(fresh, &app) {
        if installed {
            let _ = fs::rename(&previous, &app);
        }
        return Err(error);
    }
    let _ = remove(&previous);
    Ok(())
}

/// Removes a file, a link or a folder with its contents. Nothing to remove
/// is not an error.
fn remove(path: &Path) -> io::Result<()> {
    let removed = match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(error) => Err(error),
    };
    match removed {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

/// Copies a folder with its files, links and permission bits. An AppImage
/// links to its program and its icon with relative links, which stay valid
/// in the copy.
fn copy_tree(from: &Path, to: &Path) -> io::Result<()> {
    fs::create_dir(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let (source, target) = (entry.path(), to.join(entry.file_name()));
        let kind = entry.file_type()?;
        if kind.is_dir() {
            copy_tree(&source, &target)?;
        } else if kind.is_symlink() {
            symlink(fs::read_link(&source)?, &target)?;
        } else {
            fs::copy(&source, &target)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A temporary folder that is removed however the test ends.
    struct Root(PathBuf);

    impl Root {
        fn new(name: &str) -> Self {
            let root =
                std::env::temp_dir().join(format!("installed-{}-{name}", std::process::id()));
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(&root).unwrap();
            Self(root)
        }

        /// The files of an AppImage with this text as its program.
        fn files(&self, name: &str, program: &str) -> PathBuf {
            let files = self.0.join(name);
            fs::create_dir_all(files.join(PROGRAM_DIR)).unwrap();
            let path = files.join(PROGRAM_DIR).join("ipkvm-os-services");
            fs::write(&path, program).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
            symlink("usr/bin/ipkvm-os-services", files.join(APP_RUN)).unwrap();
            fs::write(files.join("notes.txt"), "notes").unwrap();
            files
        }
    }

    impl Drop for Root {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn an_install_copies_the_files_and_replaces_the_earlier_ones() {
        let root = Root::new("install");
        let folder = root.0.join("data/ipkvm-os-services");
        install_files(
            &folder,
            &root.files("first", "release 1"),
            Publisher::Set(Some(b"key".to_vec())),
        )
        .unwrap();

        let program = program(&folder);
        assert_eq!(fs::read_to_string(&program).unwrap(), "release 1");
        // The link is copied as a link, and the program keeps its mode.
        assert_eq!(
            fs::read_link(&program).unwrap(),
            Path::new("usr/bin/ipkvm-os-services")
        );
        let mode = fs::metadata(&program).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755);
        assert_eq!(
            publisher(&folder).unwrap().as_deref(),
            Some(b"key".as_slice())
        );

        // An unsigned build replaces a signed one, and its files have no
        // publisher.
        let second = root.files("second", "build 2");
        fs::remove_file(second.join("notes.txt")).unwrap();
        install_files(&folder, &second, Publisher::Set(None)).unwrap();
        assert_eq!(fs::read_to_string(&program).unwrap(), "build 2");
        assert_eq!(publisher(&folder), Ok(None));

        // Files that carry their publisher keep it.
        fs::write(second.join(PUBLISHER), "carried").unwrap();
        install_files(&folder, &second, Publisher::Carried).unwrap();
        assert_eq!(
            publisher(&folder).unwrap().as_deref(),
            Some(b"carried".as_slice())
        );
        // A key block that is there and cannot be read is not "no publisher".
        fs::remove_file(folder.join(APP).join(PUBLISHER)).unwrap();
        fs::create_dir(folder.join(APP).join(PUBLISHER)).unwrap();
        publisher(&folder).unwrap_err();
        assert!(!folder.join(APP).join("notes.txt").exists());
        let left: Vec<_> = fs::read_dir(&folder)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(left, ["app"]);
    }

    #[test]
    fn files_that_cannot_be_copied_leave_the_installed_files() {
        let root = Root::new("failed");
        let folder = root.0.join("data/ipkvm-os-services");
        install_files(
            &folder,
            &root.files("first", "release 1"),
            Publisher::Carried,
        )
        .unwrap();
        install_files(&folder, &root.0.join("missing"), Publisher::Carried).unwrap_err();
        assert_eq!(fs::read_to_string(program(&folder)).unwrap(), "release 1");
        assert!(!folder.join("app.new").exists());
    }

    #[test]
    fn the_folder_is_found_from_the_place_of_the_files() {
        let installed = Path::new("/home/u/data/ipkvm-os-services/app");
        assert_eq!(
            installation_of(installed).as_deref(),
            Some(Path::new("/home/u/data/ipkvm-os-services"))
        );
        // Files that an AppImage unpacked or mounted to run.
        assert_eq!(
            installation_of(Path::new("/tmp/appimage_extracted_1f/")),
            None
        );
        assert_eq!(installation_of(Path::new("/tmp/.mount_IP-KVMab12")), None);
        assert_eq!(installation_of(Path::new("/home/u/other/app")), None);
    }

    #[test]
    fn only_an_x86_64_program_file_is_runnable() {
        let root = Root::new("runnable");
        let files = root.files("files", "text");
        let program = files.join(PROGRAM_DIR).join("ipkvm-os-services");
        // The start of an ELF header: 64-bit, little-endian, an executable
        // for machine 62 (x86-64).
        let mut header = b"\x7fELF\x02\x01\x01\0\0\0\0\0\0\0\0\0\x03\0\x3e\0".to_vec();
        assert!(check_runnable(&program).is_err(), "a text file");
        fs::write(&program, &header).unwrap();
        check_runnable(&program).unwrap();
        // The link to the program is not the program.
        assert!(check_runnable(&files.join(APP_RUN)).is_err());
        fs::set_permissions(&program, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(check_runnable(&program).is_err(), "not executable");
        fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
        // Machine 183 is arm64.
        header[18] = 183;
        fs::write(&program, &header).unwrap();
        assert!(check_runnable(&program).is_err(), "arm64");
        assert!(check_runnable(&files.join("missing")).is_err());
    }
}
