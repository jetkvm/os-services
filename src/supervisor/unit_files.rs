//! The service definitions the installers write.

/// The systemd user unit's name.
#[cfg(target_os = "linux")]
pub const SYSTEMD_UNIT: &str = "ipkvm-os-services.service";

/// The systemd user unit that starts the sender with the graphical session.
/// It skips system accounts, such as the display manager's login screen,
/// which would otherwise claim the KVM before anyone logs in.
#[cfg(any(target_os = "linux", test))]
pub fn systemd_unit(executable: &str) -> Result<String, &'static str> {
    // systemd refuses a command path that contains one of these characters,
    // with or without an escape.
    if executable.contains(|c: char| c.is_ascii_control() || matches!(c, '"' | '\'' | '\\')) {
        return Err(
            "systemd cannot start a program whose path contains a quote, a backslash or a control character",
        );
    }
    // systemd expands percent specifiers in the command path. It does not
    // expand dollar variables there, so a dollar sign stands for itself.
    let quoted = executable.replace('%', "%%");
    Ok(format!(
        "[Unit]
Description=IP-KVM OS Services
After=graphical-session.target
PartOf=graphical-session.target
ConditionUser=!@system

[Service]
ExecStart=\"{quoted}\" --run
Restart=on-failure
RestartSec=2
KillMode=mixed
TimeoutStopSec=10

[Install]
WantedBy=graphical-session.target
"
    ))
}

/// The variables of the desktop session that the service needs, which the
/// user manager does not always have.
#[cfg(target_os = "linux")]
pub const SESSION_VARIABLES: [&str; 4] = [
    "DISPLAY",
    "XAUTHORITY",
    "WAYLAND_DISPLAY",
    "XDG_SESSION_TYPE",
];

/// The file name of the XDG autostart entry in `~/.config/autostart`.
#[cfg(target_os = "linux")]
pub const AUTOSTART_FILE: &str = "ipkvm-os-services.desktop";

/// The XDG autostart entry that starts the unit at login. GNOME and KDE also
/// start the unit through graphical-session.target, but Xfce, MATE and
/// Cinnamon do not start that target. Every desktop runs autostart entries.
/// A window manager alone, such as sway or i3, runs neither.
#[cfg(target_os = "linux")]
pub fn autostart_entry() -> String {
    format!(
        "[Desktop Entry]
Type=Application
Name=IP-KVM OS Services
Exec=sh -c \"systemctl --user import-environment {variables}; systemctl --user start {SYSTEMD_UNIT}\"
NoDisplay=true
",
        variables = SESSION_VARIABLES.join(" ")
    )
}

/// The XDG autostart entry on a computer without systemd. It starts the
/// program itself, which then keeps the service running (see `keeper`).
#[cfg(any(target_os = "linux", test))]
pub fn autostart_entry_without_systemd(program: &str) -> Result<String, &'static str> {
    Ok(format!(
        "[Desktop Entry]
Type=Application
Name=IP-KVM OS Services
Exec={program} --run
NoDisplay=true
",
        program = exec_argument(program)?
    ))
}

/// Whether a systemd unit starts this program.
#[cfg(any(target_os = "linux", test))]
pub fn unit_starts(unit: &str, program: &str) -> bool {
    let quoted = format!("\"{}\"", program.replace('%', "%%"));
    unit.lines()
        .filter_map(|line| line.strip_prefix("ExecStart="))
        .any(|command| command.starts_with(&quoted))
}

/// The file name of the applications menu entry in
/// `~/.local/share/applications`.
#[cfg(target_os = "linux")]
pub const MENU_FILE: &str = "ipkvm-os-services.desktop";

/// The applications menu entry of the installed program. Opening it starts
/// the program without arguments, which registers the service again and
/// restarts it.
#[cfg(any(target_os = "linux", test))]
pub fn menu_entry(program: &str, icon: &str) -> Result<String, &'static str> {
    let exec = exec_argument(program)?;
    if icon.contains(|c: char| c.is_ascii_control()) {
        return Err(CONTROL_CHARACTER);
    }
    Ok(format!(
        "[Desktop Entry]
Type=Application
Name=IP-KVM OS Services
Exec={exec}
Icon={icon}
Categories=Utility;
Terminal=false
",
        icon = icon.replace('\\', r"\\")
    ))
}

#[cfg(any(target_os = "linux", test))]
const CONTROL_CHARACTER: &str = "a desktop entry cannot name a path with a control character";

/// A program's path as the quoted first argument of the Exec key of a
/// desktop entry.
#[cfg(any(target_os = "linux", test))]
fn exec_argument(program: &str) -> Result<String, &'static str> {
    if program.contains(|c: char| c.is_ascii_control()) {
        return Err(CONTROL_CHARACTER);
    }
    // A desktop entry reads `\\` in a value as one backslash. In the quoted
    // argument of Exec, a backslash then escapes the characters that a shell
    // would expand, and `%%` is a percent sign.
    let mut exec = String::from("\"");
    for character in program.chars() {
        match character {
            '\\' => exec.push_str(r"\\\\"),
            '"' | '`' | '$' => {
                exec.push_str(r"\\");
                exec.push(character);
            }
            '%' => exec.push_str("%%"),
            other => exec.push(other),
        }
    }
    exec.push('"');
    Ok(exec)
}

#[cfg(any(target_os = "macos", test))]
pub const LAUNCHD_LABEL: &str = "com.jetkvm.os-services";

/// The launchd agent that starts the sender in the user's Aqua session.
/// Its label is also the app's bundle identifier, which
/// AssociatedBundleIdentifiers names so that Login Items shows the app.
/// ProcessType Interactive gives it the scheduling of a foreground app,
/// which live capture needs to keep its frame rate. KeepAlive also starts
/// the new program after an update (see `update::RESTART_STATUS`).
#[cfg(any(target_os = "macos", test))]
pub fn launchd_plist(executable: &str, log: &str) -> String {
    let (executable, log) = (xml_escape(executable), xml_escape(log));
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{LAUNCHD_LABEL}</string>
  <key>AssociatedBundleIdentifiers</key><string>{LAUNCHD_LABEL}</string>
  <key>ProgramArguments</key><array><string>{executable}</string><string>--run</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>
  <key>StandardErrorPath</key><string>{log}</string>
  <key>StandardOutPath</key><string>{log}</string>
  <key>LimitLoadToSessionType</key><string>Aqua</string>
  <key>ProcessType</key><string>Interactive</string>
</dict>
</plist>
"#
    )
}

#[cfg(any(target_os = "macos", test))]
fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn systemd_unit_quotes_the_executable() {
        let unit = systemd_unit("/opt/a b/100% $HOME ${HOME}/svc").unwrap();
        assert!(
            unit.contains(r#"ExecStart="/opt/a b/100%% $HOME ${HOME}/svc" --run"#),
            "{unit}"
        );
    }

    #[test]
    fn systemd_unit_refuses_a_path_that_systemd_refuses() {
        for path in [
            "/opt/a\"b/svc",
            "/opt/a'b/svc",
            "/opt/a\\b/svc",
            "/opt/a\nb/svc",
            "/opt/a\x7fb/svc",
        ] {
            assert!(systemd_unit(path).is_err(), "{path:?}");
        }
    }

    #[test]
    fn a_unit_starts_the_program_that_its_command_names() {
        let image = "/home/u/Down loads/100% x.AppImage";
        let unit = systemd_unit(image).unwrap();
        assert!(unit_starts(&unit, image));
        assert!(!unit_starts(&unit, "/home/u/Down loads/100% x"));
        assert!(!unit_starts(&unit, "/home/u/.local/share/app/AppRun"));
    }

    #[test]
    fn the_menu_entry_quotes_the_program_for_a_desktop_entry() {
        let entry = menu_entry(
            r#"/home/a b/50% $x `y` "z" \w/AppRun"#,
            r"/home/a b/i\con.png",
        )
        .unwrap();
        assert!(
            entry.contains(r#"Exec="/home/a b/50%% \\$x \\`y\\` \\"z\\" \\\\w/AppRun""#),
            "{entry}"
        );
        assert!(entry.contains(r"Icon=/home/a b/i\\con.png"), "{entry}");
        assert!(menu_entry("/home/a\nb/AppRun", "/icon.png").is_err());
    }

    #[test]
    fn the_autostart_entry_without_systemd_runs_the_program_itself() {
        let entry = autostart_entry_without_systemd("/home/a b/50%/AppRun").unwrap();
        assert!(
            entry.contains("\nExec=\"/home/a b/50%%/AppRun\" --run\n"),
            "{entry}"
        );
        assert!(autostart_entry_without_systemd("/home/a\nb/AppRun").is_err());
    }

    #[test]
    fn launchd_plist_escapes_paths() {
        let plist = launchd_plist("/Applications/A&B.app/x", "/Users/u/<log>");
        assert!(plist.contains("<string>/Applications/A&amp;B.app/x</string>"));
        assert!(plist.contains("<string>/Users/u/&lt;log&gt;</string>"));
    }
}
