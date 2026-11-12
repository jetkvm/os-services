// Package link connects the KVM's USB to the computer under test: by cable,
// or over USB/IP from a hub computer that has the KVM plugged in.
package link

import (
	"context"
	"errors"
	"fmt"
	"regexp"
	"strings"
	"time"

	"github.com/jetkvm/os-services/tools/e2e/ssh"
)

// Link is how the KVM's USB reaches the computer.
type Link interface {
	Connect(ctx context.Context) error
	Disconnect(ctx context.Context) error
	String() string
}

// Config is the "usbip" entry of a KVM: the hub that shares the KVM's USB.
type Config struct {
	// Hub is the SSH login of the computer that has the KVM plugged in.
	Hub string `json:"hub,omitempty"`
	// BusID is the KVM's USB port on the hub, as `usbip list -l` shows it.
	BusID string `json:"busid,omitempty"`
	// AttachOn is the SSH login of the computer whose kernel attaches the
	// KVM: the computer under test, or for a container the computer that
	// runs it. Empty means the computer under test.
	AttachOn string `json:"attach_on,omitempty"`
}

var busID = regexp.MustCompile(`^\d+-\d+(\.\d+)*$`)

// NewUSBIP is the USB/IP link of config for the computer at login.
func NewUSBIP(config Config, login string) (*USBIP, error) {
	if config.Hub == "" || config.BusID == "" {
		return nil, errors.New("usbip: hub and busid are required")
	}
	if !busID.MatchString(config.BusID) {
		return nil, fmt.Errorf("usbip: busid must be a USB port such as 1-2 or 3-1.4, not %q", config.BusID)
	}
	attachOn := config.AttachOn
	if attachOn == "" {
		attachOn = login
	}
	if ssh.Address(attachOn) == ssh.Address(config.Hub) {
		return nil, errors.New("usbip: the hub is the computer that attaches the KVM; use a cable")
	}
	return &USBIP{Hub: config.Hub, BusID: config.BusID, AttachOn: attachOn}, nil
}

// Cable is a KVM plugged into the computer. It needs nothing.
type Cable struct{}

func (Cable) Connect(context.Context) error    { return nil }
func (Cable) Disconnect(context.Context) error { return nil }
func (Cable) String() string                   { return "cable" }

// USBIP attaches the KVM from the hub. The attaching computer runs Linux
// (the kernel's usbip) or Windows (usbip-win2, under Git's sh). Both attach
// the KVM again each time it enumerates again (a restart, a media mount, a
// change of its USB functions). The hub must not run OS Services itself.
type USBIP struct {
	Hub, BusID, AttachOn string
}

const (
	usbipPort     = 3240
	attachTimeout = 30 * time.Second
	// usbip-win2's command-line client.
	windowsUSBIP = `"/c/Program Files/USBip/usbip.exe"`
)

func (u *USBIP) String() string { return fmt.Sprintf("USB/IP from %s port %s", u.Hub, u.BusID) }

func (u *USBIP) remote() string {
	return fmt.Sprintf("usbip://%s:%d/%s", ssh.Address(u.Hub), usbipPort, u.BusID)
}

// client is the attaching computer's scripts: attach starts attaching and
// keeps the KVM attached, attached succeeds once it is, and detach stops
// both and detaches it.
type client struct {
	attach, attached, detach string
}

// linuxClient keeps the KVM attached with a loop, since the kernel's usbip
// attaches once. The loop runs while the pid file holds its process ID, so
// it ends when the file goes or when a newer loop writes its own ID. It
// reads the file again right before it attaches, so a pass that began
// before a detach does not attach the KVM after it. No script sends a
// signal to the ID in the file: a file that stays after its loop has ended
// names no process, or a process of another program.
func (u *USBIP) linuxClient() client {
	pidFile := "/tmp/os-services-e2e-usbip-" + u.BusID + ".pid"
	stopLoop := "sudo -n rm -f " + pidFile
	keep := fmt.Sprintf(`echo $$ > %[1]s; mine() { [ "$(cat %[1]s 2>/dev/null)" = $$ ]; }; `+
		`while mine; do usbip port 2>/dev/null | %s | grep -q . || { mine && usbip attach -r %s -b %s >/dev/null 2>&1; }; sleep 1; done`,
		pidFile, PortsHolding(u.remote()), ssh.Address(u.Hub), u.BusID)
	// Debian has usbip in /usr/sbin, which only sudo's PATH has.
	ports := "sudo -n usbip port 2>/dev/null | " + PortsHolding(u.remote())
	return client{
		attach: strings.Join([]string{
			"set -e",
			"sudo -n modprobe vhci-hcd",
			stopLoop,
			"nohup sudo -n sh -c " + ssh.Quote(keep) + " </dev/null >/dev/null 2>&1 &",
		}, "\n"),
		attached: ports + " | grep -q .",
		detach:   stopLoop + "; for p in $(" + ports + `); do sudo -n usbip detach -p "$p" >/dev/null; done; true`,
	}
}

// windowsClient uses usbip-win2, whose attach keeps trying to attach the
// device until it is told to stop. The SSH login must be an administrator.
func (u *USBIP) windowsClient() client {
	device := fmt.Sprintf("-r %s -b %s", ssh.Address(u.Hub), u.BusID)
	stop := windowsUSBIP + " attach --stop " + device + " >/dev/null 2>&1"
	ports := windowsUSBIP + " port 2>/dev/null | " + PortsHolding(u.remote())
	return client{
		attach:   stop + "; " + windowsUSBIP + " attach " + device,
		attached: ports + " | grep -q .",
		detach:   stop + "; for p in $(" + ports + "); do " + windowsUSBIP + ` detach -p "$p" >/dev/null; done; true`,
	}
}

// client picks the attaching computer's scripts by its system.
func (u *USBIP) client(ctx context.Context) (client, error) {
	system, err := ssh.Run(ctx, u.AttachOn, "uname -s")
	if err != nil {
		return client{}, err
	}
	switch system = strings.TrimSpace(system); {
	case system == "Linux":
		return u.linuxClient(), nil
	case strings.HasPrefix(system, "MINGW"), strings.HasPrefix(system, "MSYS"):
		return u.windowsClient(), nil
	default:
		return client{}, fmt.Errorf("USB/IP needs a Linux or Windows computer, and %s runs %s; use a cable", u.AttachOn, system)
	}
}

// Connect binds the KVM on the hub, which drops a client that still holds
// it, and starts attaching it. The hub's usbip-host keeps the port, so the
// KVM is bound again each time it enumerates again.
func (u *USBIP) Connect(ctx context.Context) error {
	claim := strings.Join([]string{
		"set -e",
		"sudo -n modprobe usbip-host",
		"pgrep -x usbipd >/dev/null || sudo -n usbipd -D",
		fmt.Sprintf("sudo -n usbip unbind -b %s >/dev/null 2>&1 || true", u.BusID),
		fmt.Sprintf("sudo -n usbip bind -b %s", u.BusID),
	}, "\n")
	if _, err := ssh.Run(ctx, u.Hub, claim); err != nil {
		return err
	}
	c, err := u.client(ctx)
	if err != nil {
		return err
	}
	if _, err := ssh.Run(ctx, u.AttachOn, c.attach); err != nil {
		return err
	}
	deadline := time.Now().Add(attachTimeout)
	for {
		if _, err := ssh.Run(ctx, u.AttachOn, c.attached); err == nil {
			return nil
		}
		if time.Now().After(deadline) {
			return fmt.Errorf("%s did not attach %s within %s", u.AttachOn, u.remote(), attachTimeout)
		}
		select {
		case <-ctx.Done():
			return ctx.Err()
		case <-time.After(time.Second):
		}
	}
}

// Disconnect detaches the KVM and gives it back to the hub. Each step runs
// even when the one before fails.
func (u *USBIP) Disconnect(ctx context.Context) error {
	c, err := u.client(ctx)
	var detachErr error
	if err == nil {
		_, detachErr = ssh.Run(ctx, u.AttachOn, c.detach)
	}
	_, releaseErr := ssh.Run(ctx, u.Hub, fmt.Sprintf("sudo -n usbip unbind -b %s >/dev/null 2>&1 || true", u.BusID))
	return errors.Join(err, detachErr, releaseErr)
}

// PortsHolding is a filter for the output of `usbip port`: it prints the
// number of each port that holds remote, one per line. A port's block
// starts "Port 00:" and has a line that ends "-> <remote>", so 1-2 does not
// match 1-2.1 or 1-12.
func PortsHolding(remote string) string {
	return "awk -v r=" + ssh.Quote("-> "+remote) +
		` '{sub(/[ \t\r]+$/, "")} /^Port /{p=$2} length($0) >= length(r) && substr($0, length($0) - length(r) + 1) == r {sub(":", "", p); print p + 0}'`
}
