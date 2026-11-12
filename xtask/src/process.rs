//! Running other programs.

use crate::Result;
use std::process::{Command, Stdio};

/// A cargo command from the toolchain that runs this xtask.
pub fn cargo() -> Command {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let mut command = Command::new(cargo);
    command.current_dir(crate::root());
    command
}

/// Runs the command and fails unless it exits successfully.
pub fn run(command: &mut Command) -> Result {
    let status = command
        .status()
        .map_err(|error| format!("could not run {}: {error}", describe(command)))?;
    if !status.success() {
        return Err(format!("{} exited with {status}", describe(command)));
    }
    Ok(())
}

/// Runs the command and returns its standard output.
pub fn output(command: &mut Command) -> Result<String> {
    let output = command
        .stderr(Stdio::inherit())
        .output()
        .map_err(|error| format!("could not run {}: {error}", describe(command)))?;
    if !output.status.success() {
        return Err(format!(
            "{} exited with {}",
            describe(command),
            output.status
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// The command line, for an error message. The value of a `--password`
/// option is a secret, so the description leaves it out.
fn describe(command: &Command) -> String {
    let mut words = vec![command.get_program().to_string_lossy().into_owned()];
    let mut secret = false;
    for arg in command.get_args() {
        words.push(if secret {
            "***".into()
        } else {
            arg.to_string_lossy().into_owned()
        });
        secret = arg == "--password";
    }
    words.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_description_leaves_out_the_value_of_a_password_option() {
        let mut command = Command::new("xcrun");
        command.args(["notarytool", "submit", "--apple-id", "id"]);
        command.args(["--password", "secret", "--wait"]);
        assert_eq!(
            describe(&command),
            "xcrun notarytool submit --apple-id id --password *** --wait"
        );
    }
}
