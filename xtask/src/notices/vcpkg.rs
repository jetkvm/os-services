//! The vcpkg ports that the Windows and macOS builds link, and the copyright
//! file vcpkg installs for each.
//!
//! The ports are the dependencies in `vendor/vcpkg/vcpkg.json` and the ports
//! they depend on, from the `status` file of the vcpkg tree. Ports named
//! `vcpkg-*` are build helpers, so the walk does not go through them.

use super::{read, Entry};
use crate::{root, Result};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

/// The version and the dependencies of an installed port.
#[derive(Default)]
struct Port {
    version: String,
    depends: BTreeSet<String>,
}

pub(super) fn entries(installed: &Path, triplet: &str) -> Result<Vec<Entry>> {
    let ports = installed_ports(&read(&installed.join("vcpkg").join("status"))?, triplet);
    let manifest = read(&root().join("vendor").join("vcpkg").join("vcpkg.json"))?;
    let manifest: Value = serde_json::from_str(&manifest).map_err(|error| error.to_string())?;
    let roots = manifest["dependencies"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|dependency| dependency.as_str().or(dependency["name"].as_str()));
    let names = linked(&ports, roots);
    // Every build links FFmpeg. A walk that does not find it read no ports
    // for this triplet, and the notices would list no native library.
    if !names.contains("ffmpeg") {
        return Err(format!(
            "{} lists no installed ffmpeg for {triplet}",
            installed.join("vcpkg").join("status").display()
        ));
    }
    let mut entries = Vec::new();
    for name in names {
        let copyright = installed
            .join(triplet)
            .join("share")
            .join(&name)
            .join("copyright");
        entries.push(Entry {
            version: ports[&name].version.clone(),
            url: format!("https://vcpkg.io/en/package/{name}"),
            license: "see text".into(),
            texts: vec![read(&copyright)?],
            name,
        });
    }
    Ok(entries)
}

/// The installed ports for `triplet`, with the dependencies of their
/// installed features. A dependency for another triplet is a host tool.
fn installed_ports(status: &str, triplet: &str) -> BTreeMap<String, Port> {
    let mut ports: BTreeMap<String, Port> = BTreeMap::new();
    for stanza in status.split("\n\n") {
        let field = |key: &str| {
            stanza
                .lines()
                .find_map(|line| line.strip_prefix(key)?.strip_prefix(": "))
        };
        let (Some(name), Some(state)) = (field("Package"), field("Status")) else {
            continue;
        };
        if state != "install ok installed" || field("Architecture") != Some(triplet) {
            continue;
        }
        let port = ports.entry(name.to_owned()).or_default();
        if field("Feature").is_none() {
            port.version = field("Version").unwrap_or_default().to_owned();
        }
        for dependency in field("Depends").unwrap_or_default().split(',') {
            let (dependency, target) = dependency
                .trim()
                .split_once(':')
                .unwrap_or((dependency.trim(), triplet));
            let dependency = dependency.split('[').next().unwrap_or(dependency);
            if !dependency.is_empty() && target == triplet {
                port.depends.insert(dependency.to_owned());
            }
        }
    }
    ports
}

/// The installed ports reached from `roots`, without the `vcpkg-*` helpers.
fn linked<'a>(
    ports: &BTreeMap<String, Port>,
    roots: impl Iterator<Item = &'a str>,
) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    let mut pending: Vec<String> = roots.map(str::to_owned).collect();
    while let Some(name) = pending.pop() {
        if name.starts_with("vcpkg-") || found.contains(&name) {
            continue;
        }
        let Some(port) = ports.get(&name) else {
            continue;
        };
        pending.extend(port.depends.iter().cloned());
        found.insert(name);
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    const STATUS: &str = "\
Package: vcpkg-cmake
Version: 2024-04-23
Architecture: arm64-osx
Status: install ok installed

Package: pkgconf
Version: 2.5.1
Depends: vcpkg-tool-meson
Architecture: arm64-osx
Status: install ok installed

Package: vcpkg-pkgconfig-get-modules
Version: 2024-04-03
Depends: pkgconf
Architecture: arm64-osx
Status: install ok installed

Package: ffmpeg
Version: 7.1
Depends: vcpkg-cmake-get-vars, vcpkg-pkgconfig-get-modules
Architecture: arm64-osx
Status: install ok installed

Package: ffmpeg
Feature: nvcodec
Depends: ffnvcodec, nasm:x64-osx
Architecture: arm64-osx
Status: install ok installed

Package: ffnvcodec
Version: 12.1.14.0
Architecture: arm64-osx
Status: install ok installed

Package: libyuv
Version: 1857
Depends: libjpeg-turbo[core], vcpkg-cmake
Architecture: arm64-osx
Status: install ok installed

Package: libjpeg-turbo
Version: 3.1.1
Architecture: arm64-osx
Status: install ok installed

Package: aom
Version: 3.11.0
Architecture: arm64-osx
Status: purge ok not-installed
";

    #[test]
    fn the_walk_follows_features_and_skips_build_helpers_and_host_tools() {
        let ports = installed_ports(STATUS, "arm64-osx");
        let found = linked(
            &ports,
            ["ffmpeg", "libyuv", "aom", "mfx-dispatch"].into_iter(),
        );
        assert_eq!(
            found.into_iter().collect::<Vec<_>>(),
            ["ffmpeg", "ffnvcodec", "libjpeg-turbo", "libyuv"]
        );
        assert_eq!(ports["ffmpeg"].version, "7.1");
    }

    #[test]
    fn a_tree_without_ffmpeg_for_the_triplet_is_an_error() {
        let installed = std::env::temp_dir().join(format!("xtask-vcpkg-{}", std::process::id()));
        std::fs::create_dir_all(installed.join("vcpkg")).unwrap();
        std::fs::write(installed.join("vcpkg").join("status"), STATUS).unwrap();
        let result = entries(&installed, "x64-windows");
        std::fs::remove_dir_all(&installed).unwrap();
        assert!(result.is_err_and(|error| error.contains("ffmpeg")));
    }
}
