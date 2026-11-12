//! The libraries in the AppDir: the Ubuntu packages that linuxdeploy copied
//! the libraries from, with each package's copyright file and source, and
//! libdrmtap, which `packaging/libdrmtap/build.sh` stages. Also the AppImage
//! runtime, the program at the start of the AppImage file that mounts it.

use super::{read, Entry};
use crate::{process, root, Result};
use std::{collections::BTreeMap, fs, path::Path, process::Command};

pub(super) fn entries(appdir: &Path) -> Result<Vec<Entry>> {
    let libraries = appdir.join("usr").join("lib");
    let mut packages: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for library in file_names(&libraries)? {
        packages.entry(owner(&library)?).or_default().push(library);
    }
    let mut entries = vec![runtime()?, drmtap()?];
    for (package, libraries) in packages {
        entries.push(package_entry(&package, &libraries)?);
    }
    Ok(entries)
}

fn file_names(directory: &Path) -> Result<Vec<String>> {
    let entries =
        fs::read_dir(directory).map_err(|error| format!("{}: {error}", directory.display()))?;
    let mut names: Vec<String> = entries
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().is_file())
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect();
    names.sort();
    Ok(names)
}

/// The Ubuntu package that installed a library of this file name.
fn owner(library: &str) -> Result<String> {
    let found = process::output(Command::new("dpkg").args(["-S", &format!("*/{library}")]))?;
    found
        .lines()
        .find_map(|line| owner_from_line(line, library))
        .ok_or_else(|| format!("no Ubuntu package installs {library}"))
}

/// The first package of a `dpkg -S` line such as
/// `libavcodec60:amd64: /usr/lib/x86_64-linux-gnu/libavcodec.so.60`, when the
/// path names exactly this file.
fn owner_from_line(line: &str, library: &str) -> Option<String> {
    let (packages, path) = line.rsplit_once(": ")?;
    if !path.ends_with(&format!("/{library}")) {
        return None;
    }
    let package = packages.split(", ").next()?;
    Some(package.split(':').next()?.to_owned())
}

fn package_entry(package: &str, libraries: &[String]) -> Result<Entry> {
    let fields = process::output(Command::new("dpkg-query").args([
        "-W",
        "-f",
        "${Version}\t${source:Package}\t${source:Version}",
        package,
    ]))?;
    let mut fields = fields.split('\t');
    let version = fields.next().unwrap_or_default();
    let source = fields.next().unwrap_or(package);
    let source_version = fields.next().unwrap_or(version);
    let copyright = Path::new("/usr/share/doc").join(package).join("copyright");
    Ok(Entry {
        name: format!("{package} ({})", libraries.join(", ")),
        version: version.to_owned(),
        url: format!("https://launchpad.net/ubuntu/+source/{source}/{source_version}"),
        license: "see text".into(),
        texts: vec![read(&copyright)?],
    })
}

/// The AppImage runtime that linuxdeploy-plugin-appimage puts at the start of
/// the AppImage. It links musl, libfuse, squashfuse, zstd and zlib
/// statically, as its LICENSE file states.
fn runtime() -> Result<Entry> {
    let directory = root().join("packaging").join("notices");
    let files = [
        "appimage-runtime-MIT.txt",
        "musl-MIT.txt",
        "libfuse-LGPL-2.1.txt",
        "squashfuse-BSD-2-Clause.txt",
        "zstd-BSD-3-Clause.txt",
        "zlib.txt",
    ];
    Ok(Entry {
        name: "AppImage runtime".into(),
        version: "continuous".into(),
        url: "https://github.com/AppImage/type2-runtime".into(),
        license: "MIT, with musl (MIT), libfuse (LGPL-2.1), squashfuse (BSD-2-Clause), \
                  zstd (BSD-3-Clause) and zlib (Zlib)"
            .into(),
        texts: files
            .iter()
            .map(|file| read(&directory.join(file)))
            .collect::<Result<_>>()?,
    })
}

/// libdrmtap, from the license and the source URL that build.sh stages next
/// to the library.
fn drmtap() -> Result<Entry> {
    let staged = root().join("packaging").join("linux").join("drmtap");
    let version = file_names(&staged)?
        .iter()
        .find_map(|name| name.strip_prefix("libdrmtap.so.").map(str::to_owned))
        .ok_or("packaging/linux/drmtap holds no libdrmtap.so")?;
    Ok(Entry {
        name: "libdrmtap".into(),
        version,
        url: read(&staged.join("source"))?.trim().to_owned(),
        license: "MIT".into(),
        texts: vec![read(&staged.join("LICENSE"))?],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_owner_is_the_package_that_installs_this_exact_file() {
        let library = "libavcodec.so.60";
        let line = "libavcodec60:amd64: /usr/lib/x86_64-linux-gnu/libavcodec.so.60";
        assert_eq!(
            owner_from_line(line, library).as_deref(),
            Some("libavcodec60")
        );
        let longer = "libavcodec60:amd64: /usr/lib/x86_64-linux-gnu/libavcodec.so.60.31.102";
        assert_eq!(owner_from_line(longer, library), None);
        let two = "libfoo1:amd64, libfoo1:i386: /usr/lib/libfoo.so.1";
        assert_eq!(
            owner_from_line(two, "libfoo.so.1").as_deref(),
            Some("libfoo1")
        );
    }
}
