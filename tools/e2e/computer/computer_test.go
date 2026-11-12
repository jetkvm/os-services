package computer

import (
	"context"
	"os"
	"os/exec"
	"path/filepath"
	"testing"

	"github.com/jetkvm/os-services/tools/e2e/probe"
)

// The scripts are built from pieces of shell; a quoting mistake shows up
// only on a real computer, so each one must at least parse.
func TestEveryScriptParses(t *testing.T) {
	for name, commands := range systems {
		p := probes[name]
		scripts := map[string]string{
			"install":          installed("/out/a b.exe") + commands.install("AA:BB:CC:DD:EE:FF:00:11"),
			"start":            commands.start,
			"stop":             commands.stop,
			"sha256":           commands.sha256 + " '/tmp/a b'",
			"start noise":      p.start(noiseWindow),
			"stop noise":       p.stop(noiseWindow),
			"start key window": p.start(keyWindow),
			"stop key window":  p.stop(keyWindow),
			"start secret":     p.start(secretWindow),
			"stop secret":      p.stop(secretWindow),
			"window log":       p.log,
			"log":              commands.log,
		}
		if commands.startDesktop != "" {
			scripts["start desktop"] = commands.startDesktop
			scripts["stop desktop"] = commands.stopDesktop
		}
		for kind, script := range scripts {
			if output, err := exec.Command("sh", "-n", "-c", script).CombinedOutput(); err != nil {
				t.Errorf("%s %s: %v: %s", name, kind, err, output)
			}
		}
	}
}

func TestWindowPlaceIsWhatTheWindowReported(t *testing.T) {
	// Two screens side by side: the window is on the right one.
	got := windowPlace("ready 0.6 0.2 0.25 0.15 0.8 0.2 0.125 0.15\nready 0.7 0.3 0.25 0.15 0.85 0.3 0.125 0.15\nfocus lost at 10:00\n")
	want := probe.KeyWindow{
		Screen:  probe.Place{Left: 0.7, Top: 0.3, Width: 0.25, Height: 0.15},
		Desktop: probe.Place{Left: 0.85, Top: 0.3, Width: 0.125, Height: 0.15},
	}
	if got != want {
		t.Errorf("last reported place: got %+v, want %+v", got, want)
	}
	if got := windowPlace("ready\n"); got != probe.DefaultKeyWindow() {
		t.Errorf("no place reported: got %+v, want the key window's own %+v", got, probe.DefaultKeyWindow())
	}
}

// fakeSSH puts an ssh on PATH that prints $FAKE_SSH_OUTPUT for each
// command, and an ssh-keygen that does nothing, so that the lab key is no
// real key.
func fakeSSH(t *testing.T) {
	t.Helper()
	bin := t.TempDir()
	for name, script := range map[string]string{
		"ssh":        `case "$1" in -G) exit 0;; esac; printf %s "$FAKE_SSH_OUTPUT"`,
		"ssh-keygen": "exit 0",
	} {
		if err := os.WriteFile(filepath.Join(bin, name), []byte("#!/bin/sh\n"+script+"\n"), 0o755); err != nil {
			t.Fatal(err)
		}
	}
	t.Setenv("PATH", bin+string(os.PathListSeparator)+os.Getenv("PATH"))
	t.Setenv("XDG_STATE_HOME", t.TempDir())
}

func TestSHA256ReadsTheSumOrFails(t *testing.T) {
	const sum = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08"
	c, err := New(Config{SSH: "tester@192.0.2.20", OS: "linux"})
	if err != nil {
		t.Fatal(err)
	}
	fakeSSH(t)
	for output, want := range map[string]string{
		sum + "  /tmp/a b\n":          sum,
		"\\" + sum + "  /tmp/a\\nb\n": sum,
		"":                            "",
		" \n":                         "",
		"sha256sum: not a sum\n":      "",
	} {
		t.Setenv("FAKE_SSH_OUTPUT", output)
		got, err := c.SHA256(context.Background(), "/tmp/a b")
		if got != want || (err == nil) != (want != "") {
			t.Errorf("output %q: got %q and the error %v, want %q", output, got, err, want)
		}
	}
}
