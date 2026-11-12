package link

import (
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"strconv"
	"strings"
	"syscall"
	"testing"
	"time"
)

func TestUSBIPAttachesOnTheComputerUnderTestByDefault(t *testing.T) {
	usbip, err := NewUSBIP(Config{Hub: "tester@192.0.2.10", BusID: "3-1.4"}, "tester@192.0.2.20")
	if err != nil {
		t.Fatal(err)
	}
	if usbip.AttachOn != "tester@192.0.2.20" {
		t.Errorf("attach_on: got %q, want the computer under test", usbip.AttachOn)
	}
}

func TestNewUSBIPRefusesBadSettings(t *testing.T) {
	for _, test := range []struct {
		config Config
		want   string
	}{
		{Config{}, "are required"},
		{Config{Hub: "tester@192.0.2.10"}, "are required"},
		{Config{BusID: "1-2"}, "are required"},
		{Config{Hub: "tester@192.0.2.10", BusID: "001/021"}, "must be a USB port"},
		{Config{Hub: "other@192.0.2.20", BusID: "1-2"}, "use a cable"},
	} {
		_, err := NewUSBIP(test.config, "tester@192.0.2.20")
		if err == nil || !strings.Contains(err.Error(), test.want) {
			t.Errorf("%+v: got %v, want an error with %q", test.config, err, test.want)
		}
	}
}

// `usbip port` with three imports: the KVM at 1-2, a device at 1-2.1 behind
// a hub on that port, and one at 1-12.
const usbipPortOutput = `Imported USB devices
====================
Port 00: <Port in Use> at High Speed(480Mbps)
       JetKVM : USB Emulation Device (399b:0001)
       3-1 -> usbip://192.0.2.10:3240/1-2.1
           -> remote bus/dev 001/022
Port 03: <Port in Use> at High Speed(480Mbps)
       JetKVM : USB Emulation Device (399b:0001)
       3-4 -> usbip://192.0.2.10:3240/1-2 ` + "\r" + `
           -> remote bus/dev 001/021
Port 09: <Port in Use> at Super Speed(5000Mbps)
       unknown vendor : unknown product (0bda:8153)
       4-1 -> usbip://192.0.2.10:3240/1-12
           -> remote bus/dev 001/030
`

func TestPortsHoldingFindsOnlyTheExactDevice(t *testing.T) {
	for remote, want := range map[string]string{
		"usbip://192.0.2.10:3240/1-2":  "3\n",
		"usbip://192.0.2.10:3240/1-12": "9\n",
		"usbip://192.0.2.11:3240/1-2":  "",
	} {
		command := exec.Command("sh", "-c", PortsHolding(remote))
		command.Stdin = strings.NewReader(usbipPortOutput)
		got, err := command.Output()
		if err != nil {
			t.Fatal(err)
		}
		if string(got) != want {
			t.Errorf("%s: got %q, want %q", remote, got, want)
		}
	}
}

// usbip-win2 0.9.8.1's `usbip port` with the KVM attached, as the Windows
// VM printed it: ports count from 1, the remote line has no local bus ID,
// and lines end in CRLF.
const usbipWin2PortOutput = "Imported USB devices\r\n" +
	"====================\r\n" +
	"Port 01: device in use at High Speed(480Mbps)\r\n" +
	"         unknown vendor : unknown product (399b:0001)\r\n" +
	"           -> usbip://192.0.2.40:3240/1-3\r\n" +
	"           -> remote bus/dev: 001/002\r\n" +
	"           -> serial: \r\n" +
	"           -> mode: zero-copy\r\n" +
	"\r\n"

func TestPortsHoldingReadsUsbipWin2(t *testing.T) {
	for remote, want := range map[string]string{
		"usbip://192.0.2.40:3240/1-3":  "1\n",
		"usbip://192.0.2.40:3240/1-31": "",
	} {
		command := exec.Command("sh", "-c", PortsHolding(remote))
		command.Stdin = strings.NewReader(usbipWin2PortOutput)
		got, err := command.Output()
		if err != nil {
			t.Fatal(err)
		}
		if string(got) != want {
			t.Errorf("%s: got %q, want %q", remote, got, want)
		}
	}
}

// The scripts are built from pieces of shell; a quoting mistake shows up
// only on a real computer, so each one must at least parse.
func TestEveryClientScriptParses(t *testing.T) {
	u := &USBIP{Hub: "tester@192.0.2.10", BusID: "1-2", AttachOn: "tester@192.0.2.20"}
	for name, c := range map[string]client{"linux": u.linuxClient(), "windows": u.windowsClient()} {
		for kind, script := range map[string]string{"attach": c.attach, "attached": c.attached, "detach": c.detach} {
			if output, err := exec.Command("sh", "-n", "-c", script).CombinedOutput(); err != nil {
				t.Errorf("%s %s: %v: %s", name, kind, err, output)
			}
		}
	}
}

// linuxClientWithFakes is the Linux client of a port that only this test
// names, and a run function for its scripts. The scripts get a sudo that
// runs the command as this user, a modprobe that does nothing, and the
// given usbip, which finds the pid file in $PIDFILE. Cleanup stops the loop.
func linuxClientWithFakes(t *testing.T, usbip string) (c client, pidFile string, run func(script string) error) {
	t.Helper()
	bin := t.TempDir()
	for name, script := range map[string]string{
		"sudo":     `[ "$1" = -n ] && shift; exec "$@"`,
		"modprobe": "exit 0",
		"usbip":    usbip,
	} {
		if err := os.WriteFile(filepath.Join(bin, name), []byte("#!/bin/sh\n"+script+"\n"), 0o755); err != nil {
			t.Fatal(err)
		}
	}
	u := &USBIP{Hub: "tester@192.0.2.10", BusID: fmt.Sprintf("0-0.%d", os.Getpid()), AttachOn: "tester@192.0.2.20"}
	c = u.linuxClient()
	pidFile = "/tmp/os-services-e2e-usbip-" + u.BusID + ".pid"
	run = func(script string) error {
		command := exec.Command("sh", "-c", script)
		command.Env = append(os.Environ(), "PATH="+bin+":"+os.Getenv("PATH"), "PIDFILE="+pidFile)
		output, err := command.CombinedOutput()
		if err != nil {
			return fmt.Errorf("%v: %s", err, output)
		}
		return nil
	}
	t.Cleanup(func() {
		run(c.detach)
		os.Remove(pidFile)
	})
	return c, pidFile, run
}

// noDevice is a usbip that has no device and attaches none.
const noDevice = "exit 0"

// loopPID waits for the loop that attach started and returns its process ID.
func loopPID(t *testing.T, pidFile string) int {
	t.Helper()
	for deadline := time.Now().Add(5 * time.Second); time.Now().Before(deadline); time.Sleep(20 * time.Millisecond) {
		content, _ := os.ReadFile(pidFile)
		if pid, err := strconv.Atoi(strings.TrimSpace(string(content))); err == nil && syscall.Kill(pid, 0) == nil {
			return pid
		}
	}
	t.Fatalf("no loop runs for %s", pidFile)
	return 0
}

// waitGone waits until the process pid has ended.
func waitGone(t *testing.T, pid int, what string) {
	t.Helper()
	for deadline := time.Now().Add(5 * time.Second); time.Now().Before(deadline); time.Sleep(20 * time.Millisecond) {
		if syscall.Kill(pid, 0) != nil {
			return
		}
	}
	t.Errorf("%s: the loop %d still runs", what, pid)
}

func TestLinuxAttachAndDetachStopTheLoop(t *testing.T) {
	c, pidFile, run := linuxClientWithFakes(t, noDevice)
	if err := run(c.attach); err != nil {
		t.Fatalf("attach: %v", err)
	}
	first := loopPID(t, pidFile)
	if err := run(c.attach); err != nil {
		t.Fatalf("second attach: %v", err)
	}
	waitGone(t, first, "second attach")
	second := loopPID(t, pidFile)
	if err := run(c.detach); err != nil {
		t.Fatalf("detach: %v", err)
	}
	waitGone(t, second, "detach")
}

// A detach can arrive while a pass of the loop looks for the port. The
// usbip here stands for that detach: its port command removes the pid file
// and shows no port. The pass must not attach the KVM after it.
func TestLinuxLoopDoesNotAttachAfterADetach(t *testing.T) {
	c, pidFile, run := linuxClientWithFakes(t,
		`case "$1" in port) rm -f "$PIDFILE"; : > "$PIDFILE.port";; attach) : > "$PIDFILE.attached";; esac`)
	t.Cleanup(func() {
		os.Remove(pidFile + ".port")
		os.Remove(pidFile + ".attached")
	})
	if err := run(c.attach); err != nil {
		t.Fatalf("attach: %v", err)
	}
	for deadline := time.Now().Add(5 * time.Second); ; time.Sleep(20 * time.Millisecond) {
		if _, err := os.Stat(pidFile + ".port"); err == nil {
			break
		}
		if time.Now().After(deadline) {
			t.Fatal("the loop did not look for the port")
		}
	}
	// The pass attaches right after it looks for the port, or not at all.
	time.Sleep(300 * time.Millisecond)
	if _, err := os.Stat(pidFile + ".attached"); err == nil {
		t.Error("the loop attached the KVM after the detach")
	}
}

// A pid file can stay when its loop has ended, for example after a crash.
// The ID in it then belongs to no process, or to a process of another
// program.
func TestLinuxScriptsLeaveTheProcessOfAStalePidFile(t *testing.T) {
	t.Run("no process", func(t *testing.T) {
		c, pidFile, run := linuxClientWithFakes(t, noDevice)
		ended := exec.Command("true")
		if err := ended.Run(); err != nil {
			t.Fatal(err)
		}
		if err := os.WriteFile(pidFile, []byte(strconv.Itoa(ended.Process.Pid)+"\n"), 0o644); err != nil {
			t.Fatal(err)
		}
		if err := run(c.attach); err != nil {
			t.Fatalf("attach: %v", err)
		}
		loopPID(t, pidFile)
	})
	for name, script := range map[string]func(client) string{
		"attach": func(c client) string { return c.attach },
		"detach": func(c client) string { return c.detach },
	} {
		t.Run(name+" with the process of another program", func(t *testing.T) {
			c, pidFile, run := linuxClientWithFakes(t, noDevice)
			other := exec.Command("sleep", "60")
			if err := other.Start(); err != nil {
				t.Fatal(err)
			}
			exited := make(chan error, 1)
			go func() { exited <- other.Wait() }()
			defer other.Process.Kill()
			if err := os.WriteFile(pidFile, []byte(strconv.Itoa(other.Process.Pid)+"\n"), 0o644); err != nil {
				t.Fatal(err)
			}
			if err := run(script(c)); err != nil {
				t.Errorf("%s: %v", name, err)
			}
			select {
			case err := <-exited:
				t.Errorf("%s ended the process of another program: %v", name, err)
			case <-time.After(200 * time.Millisecond):
			}
		})
	}
}
