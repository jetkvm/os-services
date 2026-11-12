// Package computer controls the computer under test over SSH: it installs a
// build of OS Services, starts and stops it, and reads the files it wrote.
package computer

import (
	"context"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"regexp"
	"strings"

	"github.com/jetkvm/os-services/tools/e2e/ssh"
)

// Config is a computer of a lab file.
type Config struct {
	SSH string `json:"ssh"`
	OS  string `json:"os"`
	// OwnKVM is a KVM that is plugged into the computer itself.
	OwnKVM *OwnKVM `json:"own_kvm,omitempty"`
	// Skip names the checks of e2e that cannot run on this computer.
	Skip []string `json:"skip,omitempty"`
}

// OwnKVM is how to switch off a computer's own KVM: a Linux USB port, the
// Windows device of the KVM's OS Services interface, or the USB device of a
// Proxmox VM.
type OwnKVM struct {
	USBPort string `json:"usb_port,omitempty"`
	// WindowsDevice is a device instance ID, as pnputil prints it. Only this
	// interface goes: the keyboard and mouse of the KVM stay.
	WindowsDevice string `json:"windows_device,omitempty"`
	Proxmox       string `json:"proxmox,omitempty"`
	VM            int    `json:"vm,omitempty"`
	USB           string `json:"usb,omitempty"`
}

// New is the computer of config.
func New(config Config) (*Computer, error) {
	commands, ok := systems[config.OS]
	if !ok {
		return nil, fmt.Errorf("computer: os must be linux, macos or windows, not %q", config.OS)
	}
	if config.SSH == "" {
		return nil, fmt.Errorf("computer: ssh is required")
	}
	if own := config.OwnKVM; own != nil && own.USBPort == "" && own.WindowsDevice == "" && (own.Proxmox == "" || own.VM == 0 || own.USB == "") {
		return nil, fmt.Errorf("computer: own_kvm needs usb_port, windows_device, or proxmox, vm and usb")
	}
	commands.probes = probes[config.OS]
	return &Computer{login: config.SSH, commands: commands, own: config.OwnKVM, skip: config.Skip}, nil
}

// Computer is the computer under test.
type Computer struct {
	login    string
	commands system
	own      *OwnKVM
	skip     []string
	// installer is the file that Install installed.
	installer string
}

// Skip names the checks of e2e that the lab file leaves out for this
// computer.
func (c *Computer) Skip() []string { return c.skip }

// ReleaseOwnKVM switches off the computer's own KVM, if it has one, so that
// the service uses the one attached over USB/IP. The returned function
// switches it on again.
func (c *Computer) ReleaseOwnKVM(ctx context.Context) (func(context.Context) error, error) {
	switch own := c.own; {
	case own == nil:
		return func(context.Context) error { return nil }, nil
	case own.USBPort != "":
		authorize := func(ctx context.Context, value string) error {
			_, err := ssh.Run(ctx, c.login, fmt.Sprintf("echo %s | sudo -n tee /sys/bus/usb/devices/%s/authorized >/dev/null", value, own.USBPort))
			return err
		}
		return func(ctx context.Context) error { return authorize(ctx, "1") }, authorize(ctx, "0")
	case own.WindowsDevice != "":
		device := func(ctx context.Context, action string) error {
			_, err := ssh.Run(ctx, c.login, windowsShell+fmt.Sprintf("pnputil /%s-device %s >/dev/null", action, ssh.Quote(own.WindowsDevice)))
			return err
		}
		return func(ctx context.Context) error { return device(ctx, "enable") }, device(ctx, "disable")
	default:
		// Unplug the VM's USB device and plug the same one in again later.
		value, err := ssh.Run(ctx, own.Proxmox, fmt.Sprintf("qm config %d | sed -n 's/^%s: //p'", own.VM, own.USB))
		if value = strings.TrimSpace(value); err != nil || value == "" {
			return nil, fmt.Errorf("own_kvm: VM %d has no %s: %v", own.VM, own.USB, err)
		}
		if _, err := ssh.Run(ctx, own.Proxmox, fmt.Sprintf("qm set %d --delete %s", own.VM, own.USB)); err != nil {
			return nil, err
		}
		return func(ctx context.Context) error {
			_, err := ssh.Run(ctx, own.Proxmox, fmt.Sprintf("qm set %d --%s %s", own.VM, own.USB, ssh.Quote(value)))
			return err
		}, nil
	}
}

var sha256Hex = regexp.MustCompile(`^[0-9a-f]{64}$`)

// installed is where Install puts the installer on the computer: $dir/$f.
func installed(file string) string {
	return `dir="$HOME/.local/share/os-services-e2e"; f="$dir"/` + ssh.Quote(filepath.Base(file)) + `; `
}

// Install copies the installer in file to the computer, installs it,
// approves the KVM with fingerprint and registers the service. The service
// is stopped afterwards.
func (c *Computer) Install(ctx context.Context, file, fingerprint string) error {
	data, err := os.Open(file)
	if err != nil {
		return err
	}
	defer data.Close()
	// The copy goes to a new file first: a service that still runs the file
	// from an earlier run keeps it open, and Linux refuses to write a
	// running program, but not to rename over it.
	upload := "set -e; " + installed(file) + `mkdir -p "$dir"; cat > "$f.part"; mv -f "$f.part" "$f"`
	if err := ssh.Stream(ctx, c.login, upload, data, io.Discard, io.Discard); err != nil {
		return fmt.Errorf("copy %s: %w", filepath.Base(file), err)
	}
	if _, err = ssh.Run(ctx, c.login, "set -e; "+installed(file)+c.commands.install(fingerprint)); err != nil {
		return err
	}
	c.installer = file
	return nil
}

// Approve adds the KVM with fingerprint to the user's approved list, as the
// user does with --approve-kvm.
func (c *Computer) Approve(ctx context.Context, fingerprint string) error {
	return c.program(ctx, "--approve-kvm", fingerprint)
}

// Forget removes the KVM with fingerprint from the user's approved list, as
// the user does with --forget-kvm. The service keeps running.
func (c *Computer) Forget(ctx context.Context, fingerprint string) error {
	return c.program(ctx, "--forget-kvm", fingerprint)
}

// program runs the installed program with a flag and its value.
func (c *Computer) program(ctx context.Context, flag, value string) error {
	if c.installer == "" {
		return fmt.Errorf("%s: nothing is installed", c.login)
	}
	_, err := ssh.Run(ctx, c.login, "set -e; "+installed(c.installer)+c.commands.program+`"$exe" `+flag+" "+ssh.Quote(value))
	return err
}

// StartDesktop starts the computer's desktop on the display that the KVM
// captures, for a computer that starts it on request: a container of
// distros/ has e2e-desktop.service. Elsewhere it does nothing. The returned
// function stops the desktop, also after a start that failed halfway, for
// example one that systemd carried out after the SSH connection broke.
func (c *Computer) StartDesktop(ctx context.Context) (func(context.Context) error, error) {
	if c.commands.startDesktop == "" {
		return func(context.Context) error { return nil }, nil
	}
	stop := func(ctx context.Context) error {
		_, err := ssh.Run(ctx, c.login, c.commands.stopDesktop)
		return err
	}
	_, err := ssh.Run(ctx, c.login, c.commands.startDesktop)
	return stop, err
}

func (c *Computer) Start(ctx context.Context) error {
	_, err := ssh.Run(ctx, c.login, c.commands.start)
	return err
}

func (c *Computer) Stop(ctx context.Context) error {
	_, err := ssh.Run(ctx, c.login, c.commands.stop)
	return err
}

// SHA256 is the SHA-256 of a file, in hex.
func (c *Computer) SHA256(ctx context.Context, file string) (string, error) {
	output, err := ssh.Run(ctx, c.login, c.commands.sha256+" "+ssh.Quote(file))
	if err != nil {
		return "", err
	}
	// Some tools mark a path with a backslash by beginning the line with one.
	sum := ""
	if fields := strings.Fields(output); len(fields) > 0 {
		sum = strings.TrimPrefix(fields[0], `\`)
	}
	if !sha256Hex.MatchString(sum) {
		return "", fmt.Errorf("%s: no SHA-256 for %s: %q", c.login, file, output)
	}
	return sum, nil
}

func (c *Computer) Remove(ctx context.Context, file string) error {
	_, err := ssh.Run(ctx, c.login, "rm -f -- "+ssh.Quote(file))
	return err
}

// Log is the end of the service's log.
func (c *Computer) Log(ctx context.Context) (string, error) {
	return ssh.Run(ctx, c.login, c.commands.log)
}

// system is the commands of one operating system.
type system struct {
	// install installs the installer in $f.
	install func(fingerprint string) string
	// program sets $exe to the installed program, from $dir and $f.
	program string
	start   string
	stop    string
	sha256  string
	// log prints the end of the service's log.
	log string
	// startDesktop and stopDesktop start and stop a desktop that runs only
	// on request.
	startDesktop string
	stopDesktop  string
	probes       probeCommands
}

const (
	linuxUnit = "ipkvm-os-services.service"
	// Whether systemd runs the computer. Without systemd, the service has
	// no unit: its own keeper process runs it (src/supervisor/keeper.rs).
	linuxSystemd = `[ -d /run/systemd/system ]`
	// The systemd user manager of the login's user and its desktop session.
	linuxSession = `if ` + linuxSystemd + `; then export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"; ` +
		`export DBUS_SESSION_BUS_ADDRESS="${DBUS_SESSION_BUS_ADDRESS:-unix:path=$XDG_RUNTIME_DIR/bus}"; fi; `
	// The installed program, which the keeper runs with --run.
	linuxProgram = `"$HOME/.local/share/ipkvm-os-services/app/AppRun"`
	// The keeper ends the service and then itself. pkill -x compares the
	// whole command line, so it does not match the shell of this script.
	linuxKeeperStop = `k="$HOME/.local/share/ipkvm-os-services/app/AppRun --run"; pkill -xf "$k" || true; ` +
		`i=0; while pgrep -xf "$k" >/dev/null && [ $i -lt 150 ]; do sleep 0.1; i=$((i + 1)); done`
	// --install-linux-access adds the user to ipkvm-capture, which the
	// running user manager gets only at the next sign-in. A container has no
	// sign-in, so its user manager starts again; a desktop must sign in again.
	// systemd 249 (Ubuntu 22.04) can fail to put the new user manager in the
	// cgroup of the old one while that cgroup is still being removed, so the
	// start runs once more. Without systemd, the SSH login that starts the
	// service has the group.
	linuxCaptureGroup = `if ` + linuxSystemd + `; then g=$(getent group ipkvm-capture | cut -d: -f3); ` +
		`m=$(systemctl show -p MainPID --value user@$(id -u).service); ` +
		`if [ -n "$g" ] && ! grep -Eq "^Groups:.*[[:space:]]$g([[:space:]]|$)" /proc/$m/status; then ` +
		`if systemd-detect-virt -cq; then u=user@$(id -u).service; sudo -n systemctl restart $u || { sleep 1; sudo -n systemctl start $u; }; ` +
		`else echo "sign out and in again, so that the service has the ipkvm-capture group" >&2; exit 1; fi; fi; fi; `
	// The unit of a release that runs from its AppImage can leave the image's
	// FUSE mount behind at a stop, and the install here runs the AppImage,
	// which needs a free mount. A mount that tester
	// cannot remove, such as one of the install's sudo run, stays: under
	// set -e, a failed last command of the list would end the script.
	linuxRemoveMounts = `findmnt -rno TARGET 2>/dev/null | while read -r t; do case "$t" in /tmp/.mount_IP-KVM*) ` +
		`fusermount3 -uz "$t" 2>/dev/null || fusermount -uz "$t" 2>/dev/null || true; rmdir "$t" 2>/dev/null || true;; esac; done; true; `

	macosAgent = `t=gui/$(id -u)/com.jetkvm.os-services; p="$HOME/Library/LaunchAgents/com.jetkvm.os-services.plist"; `
	macosStop  = macosAgent + `launchctl bootout $t 2>/dev/null || true; i=0; ` +
		`while launchctl print $t >/dev/null 2>&1 && [ $i -lt 300 ]; do sleep 0.1; i=$((i + 1)); done`

	// Git's sh on Windows must not rewrite the arguments.
	windowsShell = "export MSYS_NO_PATHCONV=1 MSYS2_ARG_CONV_EXCL='*'; "
	windowsTask  = windowsShell + `task="IP-KVM OS Services $("$SYSTEMROOT/System32/whoami.exe" /user /fo csv /nh | tr -d '"\r' | cut -d, -f2)"; ` +
		`exe="$LOCALAPPDATA/IP-KVM OS Services/ipkvm-os-services.exe"; `
	windowsStop = windowsTask + `"$exe" --stop >/dev/null 2>&1 || true; schtasks /end /tn "$task" >/dev/null 2>&1 || true; ` +
		`taskkill /f /im ipkvm-os-services.exe >/dev/null 2>&1 || true`
)

// linuxEither is the command for a computer that systemd runs, or the one
// for a computer without systemd.
func linuxEither(systemd, other string) string {
	return `if ` + linuxSystemd + `; then ` + systemd + `; else ` + other + `; fi`
}

// linuxDesktop starts or stops the desktop of a container of distros/: a
// unit, or the e2e-desktop script in a container without systemd.
func linuxDesktop(action string) string {
	return `if [ -x /usr/local/sbin/e2e-desktop ]; then sudo -n /usr/local/sbin/e2e-desktop ` + action + `; ` +
		`elif systemctl cat e2e-desktop.service >/dev/null 2>&1; then sudo -n systemctl ` + action + ` e2e-desktop.service; fi`
}

var systems = map[string]system{
	"linux": {
		install: func(fingerprint string) string {
			return linuxSession + `app="$f"; chmod 755 "$app"; ` +
				linuxEither(`systemctl --user stop `+linuxUnit+` 2>/dev/null || true`, linuxKeeperStop) + `; ` + linuxRemoveMounts +
				`"$app" --approve-kvm ` + ssh.Quote(fingerprint) + `; ` +
				`sudo -n "$app" --install-linux-access >/dev/null; ` + linuxCaptureGroup +
				`"$app" --install-service; ` + linuxEither(`systemctl --user stop `+linuxUnit, linuxKeeperStop) + `; ` + linuxRemoveMounts
		},
		program: linuxSession + `exe="$f"; `,
		// Without systemd, the installed program starts its keeper, as the
		// applications menu entry does.
		start: linuxSession + linuxRemoveMounts + linuxEither(
			"systemctl --user reset-failed "+linuxUnit+" 2>/dev/null; systemctl --user start "+linuxUnit,
			linuxProgram+" --install-service"),
		stop:   linuxSession + linuxEither("systemctl --user stop "+linuxUnit, linuxKeeperStop),
		sha256: "sha256sum",
		log: linuxSession + linuxEither(
			"journalctl --user -u "+linuxUnit+" -n 400 -o cat --no-pager",
			`tail -n 400 "$HOME/.local/state/ipkvm-os-services/service.log"`),
		startDesktop: linuxDesktop("start"),
		stopDesktop:  linuxDesktop("stop"),
	},
	"macos": {
		// The agent runs in the session that has the screen.
		install: func(fingerprint string) string {
			return `[ "$(stat -f %Su /dev/console)" = "$(id -un)" ] || { echo "$(id -un) is not signed in at the screen" >&2; exit 1; }; ` +
				`app="$dir/unpacked"; ` + macosStop + `; rm -rf "$app"; mkdir "$app"; tar -xzf "$f" -C "$app"; ` +
				`exe=$(echo "$app"/*.app/Contents/MacOS/ipkvm-os-services); ` +
				`"$exe" --approve-kvm ` + ssh.Quote(fingerprint) + `; "$exe" --install-service; ` + macosStop
		},
		program: `exe=$(echo "$dir"/unpacked/*.app/Contents/MacOS/ipkvm-os-services); `,
		start:   macosAgent + `if launchctl print $t >/dev/null 2>&1; then launchctl kickstart $t; else launchctl bootstrap gui/$(id -u) "$p"; fi`,
		stop:    macosStop,
		sha256:  "shasum -a 256",
		log:     `tail -n 400 "$HOME/Library/Logs/IP-KVM OS Services/service.log"`,
	},
	"windows": {
		// The installer runs without questions over what is installed.
		install: func(fingerprint string) string {
			return windowsStop + `; "$f" /S; ` + windowsTask +
				`"$exe" --approve-kvm ` + ssh.Quote(fingerprint) + `; "$exe" --install-service; ` + windowsStop
		},
		program: windowsTask,
		start:   windowsTask + `schtasks /run /tn "$task" >/dev/null`,
		stop:    windowsStop,
		sha256:  "sha256sum",
		log:     `tail -n 400 "$LOCALAPPDATA/IP-KVM OS Services/logs/service.log"`,
	},
}
