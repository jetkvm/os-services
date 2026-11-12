//! Formatting and static checks. Clippy treats the function size limits in
//! clippy.toml as errors, which keeps every function short and simple.

use crate::{process, root, Options, Result};
use std::process::Command;

pub fn lint(options: Options) -> Result {
    process::run(process::cargo().args(["fmt", "--all", "--check"]))?;
    clippy()?;
    if options.skip_go {
        return Ok(());
    }
    let tools = root().join("tools");
    let unformatted =
        process::output(Command::new("gofmt").arg("-l").arg(".").current_dir(&tools))?;
    if !unformatted.trim().is_empty() {
        return Err(format!("gofmt would change:\n{unformatted}"));
    }
    process::run(
        Command::new("go")
            .args(["vet", "./..."])
            .current_dir(&tools),
    )
}

/// Runs clippy and fails on any warning.
fn clippy() -> Result {
    process::run(process::cargo().args([
        "clippy",
        "--locked",
        "--workspace",
        "--all-targets",
        "--",
        "-D",
        "warnings",
    ]))
}
