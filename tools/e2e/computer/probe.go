package computer

import (
	"bytes"
	"context"
	_ "embed"
	"fmt"
	"io"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"sync"
	"time"

	"github.com/jetkvm/os-services/tools/e2e/probe"
	"github.com/jetkvm/os-services/tools/e2e/ssh"
)

//go:embed noise_windows.ps1
var noiseWindows []byte

//go:embed keys_windows.ps1
var keysWindows []byte

//go:embed secret_windows.ps1
var secretWindows []byte

//go:embed probe_macos.swift
var probeMacOS []byte

// The two windows of perf (see package probe), and the helper that puts a
// secret on the clipboard, which runs as the windows do.
const (
	noiseWindow  = "noise"
	keyWindow    = "keys"
	secretWindow = "secret"
)

// StartNoiseWindow starts perf's noise window in the desktop session that
// the service captures, and returns once the window reports that it shows.
// The returned function closes it.
func (c *Computer) StartNoiseWindow(ctx context.Context) (func(context.Context) error, error) {
	return c.startProbe(ctx, noiseWindow)
}

// StartKeyWindow starts perf's key window in the desktop session that the
// service captures, and returns once the window reports that it shows. The
// returned function closes it.
func (c *Computer) StartKeyWindow(ctx context.Context) (func(context.Context) error, error) {
	return c.startProbe(ctx, keyWindow)
}

// StartSecretClipboard puts probe.SecretText on the clipboard of the
// desktop session, marked as secret as password managers mark a password,
// and returns once the helper reports that it did. The returned function
// stops the helper.
func (c *Computer) StartSecretClipboard(ctx context.Context) (func(context.Context) error, error) {
	return c.startProbe(ctx, secretWindow)
}

// KeyWindowPlace is where the key window is: what it last wrote after
// "ready", or where it asked to be when it wrote nothing more.
func (c *Computer) KeyWindowPlace(ctx context.Context) (probe.KeyWindow, error) {
	output, err := ssh.Run(ctx, c.login, c.commands.probes.log)
	return windowPlace(output), err
}

// windowPlace is the place in a window log's last "ready" line that has
// one, or the key window's own place when no line has one.
func windowPlace(log string) probe.KeyWindow {
	place := probe.DefaultKeyWindow()
	for _, line := range strings.Split(log, "\n") {
		var s, d probe.Place
		if n, _ := fmt.Sscanf(strings.TrimSpace(line), "ready %g %g %g %g %g %g %g %g",
			&s.Left, &s.Top, &s.Width, &s.Height, &d.Left, &d.Top, &d.Width, &d.Height); n == 8 {
			place = probe.KeyWindow{Screen: s, Desktop: d}
		}
	}
	return place
}

// KeyFocusLosses is how often the key window lost the keyboard focus.
func (c *Computer) KeyFocusLosses(ctx context.Context) (int, error) {
	output, err := ssh.Run(ctx, c.login, c.commands.probes.log)
	return strings.Count(output, "focus lost"), err
}

// windowStart limits how long a window may take to show. On Windows its
// script first compiles its C#, which takes seconds on a busy computer.
const windowStart = 30 * time.Second

// startProbe starts a window and waits until its log says "ready". A window
// that does not show, for example because the helper found no screen with
// the KVM's EDID, is closed, as is one whose start failed halfway.
func (c *Computer) startProbe(ctx context.Context, window string) (func(context.Context) error, error) {
	p := c.commands.probes
	helper, err := p.helper(window)
	if err != nil {
		return nil, err
	}
	stop := func(ctx context.Context) error {
		_, err := ssh.Run(ctx, c.login, p.stop(window))
		return err
	}
	fail := func(err error) (func(context.Context) error, error) {
		stop(context.WithoutCancel(ctx))
		return nil, err
	}
	if err := ssh.Stream(ctx, c.login, p.start(window), bytes.NewReader(helper), io.Discard, io.Discard); err != nil {
		return fail(fmt.Errorf("start the %s window: %w", window, err))
	}
	deadline := time.Now().Add(windowStart)
	for {
		output, err := ssh.Run(ctx, c.login, p.log)
		if err == nil && strings.Contains(output, "ready") {
			return stop, nil
		}
		if time.Now().After(deadline) {
			return fail(fmt.Errorf("the %s window did not show within %s", window, windowStart))
		}
		select {
		case <-ctx.Done():
			return fail(ctx.Err())
		case <-time.After(250 * time.Millisecond):
		}
	}
}

// windowArgs are the position, size and other settings of a window for a
// helper's command line, then the window's log, which $log names.
func windowArgs(window string) []string {
	switch window {
	case noiseWindow:
		return append(strings.Fields(probe.NoiseArgs()), `$log`)
	case secretWindow:
		return []string{probe.SecretText, `$log`}
	}
	return append(strings.Fields(probe.KeyArgs()), `$log`)
}

// x11Helper is the windows' program for X11 and Xwayland, built once for
// all Linux targets of a run.
var x11Helper struct {
	once sync.Once
	data []byte
	err  error
}

// linuxHelper builds tools/e2e/probe/x11 as a static Linux program, which
// runs on every distribution.
func linuxHelper(string) ([]byte, error) {
	x11Helper.once.Do(func() {
		top, err := exec.Command("git", "rev-parse", "--show-toplevel").Output()
		if err != nil {
			x11Helper.err = fmt.Errorf("build the X11 windows: %w", err)
			return
		}
		dir, err := os.MkdirTemp("", "os-services-e2e-probe")
		if err != nil {
			x11Helper.err = err
			return
		}
		defer os.RemoveAll(dir)
		out := filepath.Join(dir, "probe")
		build := exec.Command("go", "build", "-o", out, "./e2e/probe/x11")
		build.Dir = filepath.Join(strings.TrimSpace(string(top)), "tools")
		build.Env = append(os.Environ(), "CGO_ENABLED=0", "GOOS=linux", "GOARCH=amd64")
		if output, err := build.CombinedOutput(); err != nil {
			x11Helper.err = fmt.Errorf("build the X11 windows: %w: %s", err, output)
			return
		}
		x11Helper.data, x11Helper.err = os.ReadFile(out)
	})
	return x11Helper.data, x11Helper.err
}

// probeCommands are the commands of perf's windows on one system. start
// gets the helper on its standard input: the X11 program on Linux, the
// window's script on Windows, and the Swift source on macOS.
type probeCommands struct {
	helper func(window string) ([]byte, error)
	start  func(window string) string
	stop   func(window string) string
	// log prints the window log: "ready" once a window shows, and the key
	// window's losses of the focus.
	log string
}

const (
	probeDir      = `d="$HOME/.local/share/os-services-e2e"; mkdir -p "$d"; log="$d/window.log"; `
	macosAgentFmt = `t=gui/$(id -u)/com.jetkvm.os-services-e2e.%s; `
	macosBootout  = `launchctl bootout $t 2>/dev/null; i=0; while launchctl print $t >/dev/null 2>&1 && [ $i -lt 50 ]; do sleep 0.1; i=$((i + 1)); done; `
	macosPlistFmt = `<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>Label</key><string>com.jetkvm.os-services-e2e.%s</string>
<key>ProgramArguments</key><array>%s</array>
<key>ProcessType</key><string>Interactive</string>
</dict></plist>`
)

func probeTask(window string) string { return "IP-KVM OS Services e2e " + window }

// linuxWindowStop ends a window on a computer without systemd, by the
// process ID that its start wrote to $p.
func linuxWindowStop(window string) string {
	return `p="$HOME/.local/share/os-services-e2e/` + window + `.pid"; ` +
		`if [ -f "$p" ]; then kill "$(cat "$p")" 2>/dev/null; rm -f "$p"; fi`
}

var probes = map[string]probeCommands{
	// The user manager has the desktop's variables, so a window runs as
	// one of its units. Without systemd, the SSH login has the variables,
	// and the window runs in a session of its own.
	"linux": {
		helper: linuxHelper,
		start: func(window string) string {
			unit := "os-services-e2e-" + window
			return linuxSession + probeDir + `cat > "$d/probe.part" && chmod 755 "$d/probe.part" && mv -f "$d/probe.part" "$d/probe" && ` +
				`rm -f "$log"; ` + linuxEither(
				`systemctl --user stop `+unit+` 2>/dev/null; systemctl --user reset-failed `+unit+` 2>/dev/null; `+
					`systemd-run --user --quiet --unit=`+unit+` "$d/probe" `+window+` "$log"`,
				linuxWindowStop(window)+`; setsid "$d/probe" `+window+` "$log" </dev/null >/dev/null 2>&1 & echo $! > "$p"`)
		},
		stop: func(window string) string {
			return linuxSession + linuxEither(`systemctl --user stop os-services-e2e-`+window+` 2>/dev/null`, linuxWindowStop(window)) + `; true`
		},
		log: `cat "$HOME/.local/share/os-services-e2e/window.log" 2>/dev/null; true`,
	},
	// A window runs as an interactive task in the signed-in session.
	"windows": {
		helper: func(window string) ([]byte, error) {
			switch window {
			case noiseWindow:
				return noiseWindows, nil
			case secretWindow:
				return secretWindows, nil
			}
			return keysWindows, nil
		},
		start: func(window string) string {
			return windowsShell + `f="$HOME/os-services-e2e-` + window + `.ps1"; cat > "$f" && rm -f "$HOME/os-services-e2e-window.log" && ` +
				`log="$(cygpath -w "$HOME/os-services-e2e-window.log")" && ` +
				fmt.Sprintf(`schtasks /create /tn "%[1]s" /sc once /st 00:00 /it /f /tr `+
					`"powershell -NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -File \"$(cygpath -w "$f")\" %[2]s" >/dev/null 2>&1 && `+
					`schtasks /run /tn "%[1]s" >/dev/null`, probeTask(window), strings.ReplaceAll(strings.Join(windowArgs(window), " "), `$log`, `\"$log\"`))
		},
		stop: func(window string) string {
			return windowsShell + fmt.Sprintf(`schtasks /end /tn "%[1]s" >/dev/null 2>&1; schtasks /delete /tn "%[1]s" /f >/dev/null 2>&1; true`, probeTask(window))
		},
		log: `cat "$HOME/os-services-e2e-window.log" 2>/dev/null; true`,
	},
	// The Swift source is compiled when it changed, and a window runs as a
	// launch agent in the session that has the screen, as the service does.
	"macos": {
		helper: func(string) ([]byte, error) { return probeMacOS, nil },
		start: func(window string) string {
			var args strings.Builder
			for _, arg := range append([]string{`$d/probe`, window}, windowArgs(window)...) {
				fmt.Fprintf(&args, "<string>%s</string>", arg)
			}
			return probeDir + `cat > "$d/probe.swift.new" && ` +
				`if cmp -s "$d/probe.swift.new" "$d/probe.swift" && [ -x "$d/probe" ]; then rm "$d/probe.swift.new"; ` +
				`else mv "$d/probe.swift.new" "$d/probe.swift" && xcrun swiftc -O -o "$d/probe" "$d/probe.swift"; fi && ` +
				`rm -f "$log"; ` + fmt.Sprintf(macosAgentFmt, window) + macosBootout +
				`cat > "$d/` + window + `.plist" <<EOF` + "\n" + fmt.Sprintf(macosPlistFmt, window, args.String()) + "\nEOF\n" +
				`launchctl bootstrap gui/$(id -u) "$d/` + window + `.plist" && launchctl kickstart $t`
		},
		stop: func(window string) string {
			return probeDir + fmt.Sprintf(macosAgentFmt, window) + macosBootout + `rm -f "$d/` + window + `.plist"; true`
		},
		log: `cat "$HOME/.local/share/os-services-e2e/window.log" 2>/dev/null; true`,
	},
}
