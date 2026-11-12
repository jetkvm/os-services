//! THIRD_PARTY_NOTICES.md for the installer of the platform this runs on: the
//! Rust crates the service links, the native libraries the installer
//! carries, and the license texts of both.

mod appimage;
mod crates;
mod vcpkg;

use crate::{root, Result};
use std::{collections::HashMap, fs, path::Path};

const REPOSITORY: &str = "https://github.com/jetkvm/os-services";

/// One component and the license texts that come with it.
struct Entry {
    name: String,
    version: String,
    url: String,
    license: String,
    texts: Vec<String>,
}

/// Where the native libraries of the installer and their license texts are.
pub enum Native<'a> {
    /// The vcpkg tree that the Windows and macOS builds link statically.
    Vcpkg {
        installed: &'a Path,
        triplet: &'a str,
    },
    /// The AppDir after linuxdeploy copied the libraries into it, and the
    /// vcpkg tree that the Linux build links statically.
    AppImage {
        appdir: &'a Path,
        installed: &'a Path,
        triplet: &'a str,
    },
}

/// Writes the notices to `output`.
pub fn write(output: &Path, native: Native) -> Result {
    let own = read(&root().join("LICENSE"))?;
    let crates = crates::entries()?;
    const VCPKG_TITLE: &str = "Native libraries";
    const VCPKG_INTRO: &str = "Built with vcpkg from `vendor/vcpkg` and linked into the program.";
    let mut sections = vec![("Rust crates", "Linked into the program.", crates)];
    match native {
        Native::Vcpkg { installed, triplet } => {
            sections.push((
                VCPKG_TITLE,
                VCPKG_INTRO,
                vcpkg::entries(installed, triplet)?,
            ));
        }
        Native::AppImage {
            appdir,
            installed,
            triplet,
        } => {
            sections.push((
                VCPKG_TITLE,
                VCPKG_INTRO,
                vcpkg::entries(installed, triplet)?,
            ));
            sections.push((
                "Libraries in the AppImage",
                "Ubuntu packages that the AppImage carries, and the capture library. Each Ubuntu package links to its source.",
                appimage::entries(appdir)?,
            ));
        }
    }
    let text = render(&own, &sections);
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent).map_err(|error| format!("{}: {error}", parent.display()))?;
    }
    fs::write(output, text).map_err(|error| format!("{}: {error}", output.display()))
}

fn read(path: &Path) -> Result<String> {
    fs::read_to_string(path).map_err(|error| format!("{}: {error}", path.display()))
}

/// The license texts, each once, numbered in the order they first appear.
#[derive(Default)]
struct Texts {
    numbers: HashMap<String, usize>,
    bodies: Vec<String>,
}

impl Texts {
    fn number(&mut self, text: &str) -> usize {
        let key = text.split_whitespace().collect::<Vec<_>>().join(" ");
        *self.numbers.entry(key).or_insert_with(|| {
            self.bodies.push(text.trim_matches('\n').to_owned());
            self.bodies.len()
        })
    }
}

fn render(own: &str, sections: &[(&str, &str, Vec<Entry>)]) -> String {
    let mut texts = Texts::default();
    let own_number = texts.number(own);
    let mut out = vec![
        "# Third-party notices".to_owned(),
        String::new(),
        format!(
            "IP-KVM OS Services is licensed under the MIT License (license text \
             {own_number}). Its source code is at <{REPOSITORY}>."
        ),
        String::new(),
        "This installer also contains the third-party software below. Each part \
         keeps its own license. The license texts follow the lists."
            .to_owned(),
        String::new(),
    ];
    for (title, intro, entries) in sections {
        out.extend(render_section(title, intro, entries, &mut texts));
    }
    out.extend(["## License texts".to_owned(), String::new()]);
    for (index, body) in texts.bodies.iter().enumerate() {
        let fence = if body.contains("```") { "````" } else { "```" };
        out.push(format!("### License text {}", index + 1));
        out.extend([String::new(), format!("{fence}text"), body.clone()]);
        out.extend([fence.to_owned(), String::new()]);
    }
    out.join("\n")
}

fn render_section(title: &str, intro: &str, entries: &[Entry], texts: &mut Texts) -> Vec<String> {
    let mut out = vec![
        format!("## {title}"),
        String::new(),
        intro.to_owned(),
        String::new(),
        "| Component | Version | License | Text |".to_owned(),
        "| --- | --- | --- | --- |".to_owned(),
    ];
    for entry in entries {
        let mut numbers: Vec<usize> = entry.texts.iter().map(|t| texts.number(t)).collect();
        numbers.sort_unstable();
        numbers.dedup();
        let links: Vec<String> = numbers
            .iter()
            .map(|n| format!("[{n}](#license-text-{n})"))
            .collect();
        let links = if links.is_empty() {
            "none".to_owned()
        } else {
            links.join(", ")
        };
        out.push(format!(
            "| [{}]({}) | {} | {} | {links} |",
            entry.name, entry.url, entry.version, entry.license
        ));
    }
    out.push(String::new());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, texts: &[&str]) -> Entry {
        Entry {
            name: name.into(),
            version: "1.0".into(),
            url: format!("https://example.com/{name}"),
            license: "MIT".into(),
            texts: texts.iter().map(|t| (*t).to_owned()).collect(),
        }
    }

    #[test]
    fn a_text_that_differs_only_in_white_space_is_printed_once() {
        let entries = vec![
            entry("a", &["Permission is granted.\n"]),
            entry("b", &["Permission  is\ngranted."]),
            entry("c", &["Other terms."]),
        ];
        let out = render("Own terms.", &[("Crates", "Linked.", entries)]);
        assert_eq!(out.matches("### License text").count(), 3);
        assert!(out.contains("| [a](https://example.com/a) | 1.0 | MIT | [2](#license-text-2) |"));
        assert!(out.contains("| [b](https://example.com/b) | 1.0 | MIT | [2](#license-text-2) |"));
        assert!(out.contains("| [c](https://example.com/c) | 1.0 | MIT | [3](#license-text-3) |"));
    }

    #[test]
    fn a_component_with_the_programs_license_points_to_text_1() {
        let entries = vec![entry("forked", &["Own terms."])];
        let out = render("Own terms.", &[("Crates", "Linked.", entries)]);
        assert!(out.contains("(license text 1)"));
        assert!(out.contains(
            "| [forked](https://example.com/forked) | 1.0 | MIT | [1](#license-text-1) |"
        ));
        assert_eq!(out.matches("### License text").count(), 1);
    }

    #[test]
    fn a_text_with_a_code_fence_gets_a_longer_fence() {
        let entries = vec![entry("a", &["Use ```this``` form."])];
        let out = render("Own terms.", &[("Crates", "Linked.", entries)]);
        assert!(out.contains("````text\nUse ```this``` form.\n````"));
    }
}
