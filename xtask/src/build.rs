//! The release build of the sender and the unit tests.

use crate::{process, root, Options, Result};
use std::{
    io::{BufRead, BufReader},
    path::PathBuf,
    process::{Command, Stdio},
};

/// Builds the sender and its unit-test executable in one cargo run, so both
/// share the release build of the dependencies and link in parallel.
/// Returns the test executables.
pub fn build() -> Result<Vec<PathBuf>> {
    let mut cargo = process::cargo();
    cargo.args([
        "build",
        "--release",
        "--locked",
        "--package",
        "ipkvm-os-services",
    ]);
    cargo.args([
        "--bins",
        "--tests",
        "--message-format=json-render-diagnostics",
    ]);
    let mut child = cargo
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|error| format!("could not run cargo: {error}"))?;
    let stdout = child.stdout.take().expect("stdout is piped");
    let tests: Vec<PathBuf> = BufReader::new(stdout)
        .lines()
        .map_while(std::io::Result::ok)
        .filter_map(|line| test_executable(&line))
        .collect();
    let status = child.wait().map_err(|error| error.to_string())?;
    if !status.success() {
        return Err(format!("cargo build exited with {status}"));
    }
    if tests.is_empty() {
        return Err("cargo reported no unit-test executable".into());
    }
    Ok(tests)
}

/// The test executable a cargo JSON message reports, if it reports one.
fn test_executable(line: &str) -> Option<PathBuf> {
    let message: serde_json::Value = serde_json::from_str(line).ok()?;
    if message["reason"] != "compiler-artifact" || message["profile"]["test"] != true {
        return None;
    }
    message["executable"].as_str().map(PathBuf::from)
}

pub fn test(options: Options) -> Result {
    for executable in build()? {
        let mut command = Command::new(executable);
        if options.hardware {
            command.arg("--include-ignored");
        }
        process::run(&mut command)?;
    }
    process::run(process::cargo().args(["test", "--locked", "--package", "xtask"]))?;
    if !options.skip_go {
        process::run(
            Command::new("go")
                .args(["test", "./..."])
                .current_dir(root().join("tools")),
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_test_executables_from_cargo_messages() {
        let test = r#"{"reason":"compiler-artifact","profile":{"test":true},"executable":"/t/deps/app-1"}"#;
        let binary =
            r#"{"reason":"compiler-artifact","profile":{"test":false},"executable":"/t/app"}"#;
        let library =
            r#"{"reason":"compiler-artifact","profile":{"test":false},"executable":null}"#;
        assert_eq!(test_executable(test), Some(PathBuf::from("/t/deps/app-1")));
        assert_eq!(test_executable(binary), None);
        assert_eq!(test_executable(library), None);
        assert_eq!(test_executable("Compiling app"), None);
    }
}
