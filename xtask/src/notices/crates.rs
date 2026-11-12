//! The Rust crates that the service links on this platform, from
//! `cargo metadata`, and the license files they ship.
//!
//! A crate counts when the service reaches it through normal dependencies.
//! Build dependencies and procedural macros run only during the build, so
//! they and their dependencies do not count. The license files are the files
//! named LICENSE, LICENCE, COPYING, NOTICE or UNLICENSE in the crate
//! directory and its direct subdirectories, where a `-sys` crate keeps the C
//! library it builds. For a crate from a Git repository, the search also
//! goes up to the root of the checkout. `packaging/notices/crates.json` adds
//! texts for the crates that ship none.

use super::{read, Entry, REPOSITORY};
use crate::{process, root, Result};
use serde_json::Value;
use std::{
    collections::{BTreeMap, HashMap},
    fs,
    path::{Path, PathBuf},
    process::Command,
};

const SERVICE: &str = "ipkvm-os-services";

pub(super) fn entries() -> Result<Vec<Entry>> {
    let host = host_triple()?;
    let metadata = process::output(process::cargo().args([
        "metadata",
        "--locked",
        "--format-version",
        "1",
        "--filter-platform",
        &host,
    ]))?;
    let metadata: Value = serde_json::from_str(&metadata).map_err(|error| error.to_string())?;
    let added = notices_dir().join("crates.json");
    let added: BTreeMap<String, Vec<String>> =
        serde_json::from_str(&read(&added)?).map_err(|error| format!("crates.json: {error}"))?;
    let mut entries = Vec::new();
    let mut missing = Vec::new();
    for package in linked(&metadata)? {
        let entry = entry(package, &added)?;
        if entry.texts.is_empty() && !added.contains_key(&entry.name) {
            missing.push(format!("{} {}", entry.name, entry.version));
        }
        entries.push(entry);
    }
    if !missing.is_empty() {
        return Err(format!(
            "these crates ship no license file; add their texts to \
             packaging/notices/crates.json: {}",
            missing.join(", ")
        ));
    }
    Ok(entries)
}

fn notices_dir() -> PathBuf {
    root().join("packaging").join("notices")
}

fn host_triple() -> Result<String> {
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".into());
    let version = process::output(Command::new(rustc).arg("-vV"))?;
    version
        .lines()
        .find_map(|line| line.strip_prefix("host: "))
        .map(str::to_owned)
        .ok_or_else(|| "rustc -vV names no host".into())
}

/// The packages the service reaches through normal dependencies, without
/// the service itself and without procedural macros, sorted by name.
fn linked(metadata: &Value) -> Result<Vec<&Value>> {
    let packages: HashMap<&str, &Value> = metadata["packages"]
        .as_array()
        .ok_or("cargo metadata has no packages")?
        .iter()
        .filter_map(|package| Some((package["id"].as_str()?, package)))
        .collect();
    let nodes: HashMap<&str, &Value> = metadata["resolve"]["nodes"]
        .as_array()
        .ok_or("cargo metadata has no resolve graph")?
        .iter()
        .filter_map(|node| Some((node["id"].as_str()?, node)))
        .collect();
    let service = packages
        .values()
        .find(|package| package["name"] == SERVICE)
        .and_then(|package| package["id"].as_str())
        .ok_or("cargo metadata does not list the service")?;
    let mut found = vec![service];
    let mut next = 0;
    while let Some(id) = found.get(next).copied() {
        next += 1;
        for dependency in normal_dependencies(nodes.get(id)) {
            if !found.contains(&dependency) && !is_proc_macro(packages.get(dependency)) {
                found.push(dependency);
            }
        }
    }
    let mut linked: Vec<&Value> = found[1..]
        .iter()
        .filter_map(|id| packages.get(id).copied())
        .collect();
    linked.sort_by_key(|package| (package["name"].as_str(), package["version"].as_str()));
    Ok(linked)
}

fn normal_dependencies<'a>(node: Option<&&'a Value>) -> impl Iterator<Item = &'a str> {
    node.and_then(|node| node["deps"].as_array())
        .into_iter()
        .flatten()
        .filter(|dependency| {
            dependency["dep_kinds"]
                .as_array()
                .is_some_and(|kinds| kinds.iter().any(|kind| kind["kind"].is_null()))
        })
        .filter_map(|dependency| dependency["pkg"].as_str())
}

fn is_proc_macro(package: Option<&&Value>) -> bool {
    package
        .and_then(|package| package["targets"].as_array())
        .into_iter()
        .flatten()
        .any(|target| {
            target["kind"]
                .as_array()
                .is_some_and(|kinds| kinds.iter().any(|kind| kind == "proc-macro"))
        })
}

fn entry(package: &Value, added: &BTreeMap<String, Vec<String>>) -> Result<Entry> {
    let name = package["name"].as_str().unwrap_or_default().to_owned();
    let version = package["version"].as_str().unwrap_or_default().to_owned();
    let manifest = PathBuf::from(package["manifest_path"].as_str().unwrap_or_default());
    let directory = manifest.parent().ok_or("a manifest has no directory")?;
    let source = package["source"].as_str();
    let mut files = license_files(directory, source);
    if let Some(file) = package["license_file"].as_str() {
        files.push(directory.join(file));
    }
    let mut texts = files
        .iter()
        .map(|file| read(file))
        .collect::<Result<Vec<_>>>()?;
    for file in added.get(&name).into_iter().flatten() {
        texts.push(read(&notices_dir().join(file))?);
    }
    let license = package["license"]
        .as_str()
        .unwrap_or("not stated in Cargo.toml")
        .to_owned();
    let url = url(&name, &version, source, directory);
    Ok(Entry {
        name,
        version,
        url,
        license,
        texts,
    })
}

fn license_files(directory: &Path, source: Option<&str>) -> Vec<PathBuf> {
    let mut files = files_in(directory);
    for subdirectory in subdirectories(directory) {
        files.extend(files_in(&subdirectory));
    }
    if source.is_some_and(|source| source.starts_with("git+")) {
        let mut directory = directory;
        while files.is_empty() && !is_checkout_root(directory) {
            let Some(parent) = directory.parent() else {
                break;
            };
            directory = parent;
            files = files_in(directory);
        }
    }
    files
}

/// A Git dependency is checked out to `git/checkouts/<repository>/<revision>`
/// in the cargo home.
fn is_checkout_root(directory: &Path) -> bool {
    directory
        .parent()
        .and_then(Path::parent)
        .and_then(Path::file_name)
        .is_some_and(|name| name == "checkouts")
}

fn subdirectories(directory: &Path) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = fs::read_dir(directory)
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.is_dir())
        .collect();
    found.sort();
    found
}

fn files_in(directory: &Path) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = fs::read_dir(directory)
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.is_file() && is_license_file(path))
        .collect();
    found.sort();
    found
}

fn is_license_file(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let name = name.to_ascii_lowercase();
    let code = [".rs", ".toml", ".json", ".c", ".h"]
        .iter()
        .any(|extension| name.ends_with(extension));
    !code
        && ["license", "licence", "copying", "notice", "unlicense"]
            .iter()
            .any(|prefix| name.starts_with(prefix))
}

/// Where the source of a crate is: crates.io, the Git revision, or the
/// vendored directory in this repository.
fn url(name: &str, version: &str, source: Option<&str>, directory: &Path) -> String {
    match source {
        Some(source) if source.starts_with("git+") => {
            let source = source.trim_start_matches("git+");
            let (location, revision) = source.split_once('#').unwrap_or((source, ""));
            let location = location.split('?').next().unwrap_or(location);
            format!("{}/tree/{revision}", location.trim_end_matches(".git"))
        }
        Some(_) => format!("https://crates.io/crates/{name}/{version}"),
        None => {
            let relative = directory.strip_prefix(root()).unwrap_or(directory);
            let relative = relative.to_string_lossy().replace('\\', "/");
            format!("{REPOSITORY}/tree/main/{relative}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn package(id: &str, kind: &str) -> Value {
        json!({ "id": id, "name": id, "version": "1.0.0", "targets": [{ "kind": [kind] }] })
    }

    fn dependency(id: &str, kind: Option<&str>) -> Value {
        json!({ "pkg": id, "dep_kinds": [{ "kind": kind }] })
    }

    #[test]
    fn build_dependencies_and_procedural_macros_are_not_linked() {
        let metadata = json!({
            "packages": [
                package(SERVICE, "bin"),
                package("runtime", "lib"),
                package("derive", "proc-macro"),
                package("derive-helper", "lib"),
                package("cc", "lib"),
                package("transitive", "lib"),
            ],
            "resolve": { "nodes": [
                { "id": SERVICE, "deps": [
                    dependency("runtime", None),
                    dependency("derive", None),
                    dependency("cc", Some("build")),
                ] },
                { "id": "runtime", "deps": [dependency("transitive", None)] },
                { "id": "derive", "deps": [dependency("derive-helper", None)] },
            ] },
        });
        let names: Vec<&str> = linked(&metadata)
            .unwrap()
            .iter()
            .map(|package| package["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["runtime", "transitive"]);
    }

    #[test]
    fn a_git_crate_links_to_its_revision() {
        let source = "git+https://github.com/example/widgets?rev=f07b6e23#f07b6e2338b03ef2";
        assert_eq!(
            url("widgets", "0.1.0", Some(source), Path::new("/x")),
            "https://github.com/example/widgets/tree/f07b6e2338b03ef2"
        );
        let source = "git+https://github.com/example/gadgets.git?branch=fix#c2f74662";
        assert_eq!(
            url("gadgets-core", "0.1.5", Some(source), Path::new("/x")),
            "https://github.com/example/gadgets/tree/c2f74662"
        );
    }

    #[test]
    fn license_files_are_told_apart_from_code() {
        for name in [
            "LICENSE",
            "LICENSE-MIT",
            "license.txt",
            "COPYING",
            "UNLICENSE",
            "NOTICE",
        ] {
            assert!(is_license_file(Path::new(name)), "{name}");
        }
        for name in ["license.rs", "README.md", "Cargo.toml", "licenses.json"] {
            assert!(!is_license_file(Path::new(name)), "{name}");
        }
    }
}
