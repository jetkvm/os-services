package lab

import (
	"strings"
	"testing"

	"github.com/jetkvm/os-services/tools/e2e/computer"
	"github.com/jetkvm/os-services/tools/e2e/kvm"
	"github.com/jetkvm/os-services/tools/e2e/link"
)

func testLab() Lab {
	return Lab{
		KVMs: map[string]KVM{
			"mini-a": {Config: kvm.Config{URL: "http://192.0.2.10"}},
			"mini-b": {Config: kvm.Config{URL: "http://192.0.2.11", Password: "own"},
				USBIP: &link.Config{Hub: "tester@192.0.2.40", BusID: "1-3"}},
		},
		Computers: map[string]computer.Config{
			"n100":    {SSH: "tester@192.0.2.21", OS: "linux"},
			"windows": {SSH: "tester@192.0.2.22", OS: "windows"},
		},
		Targets: []Entry{
			{Computer: "n100", KVM: "mini-a"},
			{Computer: "windows", KVM: "mini-b", Link: "usbip"},
		},
	}
}

func TestCheckLabPairsComputersWithKVMs(t *testing.T) {
	targets, err := checkLab(testLab(), "from-environment")
	if err != nil {
		t.Fatal(err)
	}
	if targets[0].Name != "n100/mini-a" || targets[0].Link.String() != "cable" || targets[0].KVM.Password != "from-environment" {
		t.Errorf("first target: %+v", targets[0])
	}
	if !targets[1].USBIP || !strings.HasPrefix(targets[1].Link.String(), "USB/IP") || targets[1].KVM.Password != "own" {
		t.Errorf("second target: %+v", targets[1])
	}
	if got := strings.Join(Systems(targets), ","); got != "linux,windows" {
		t.Errorf("systems: %s", got)
	}
}

func TestCheckLabRefusesBrokenReferences(t *testing.T) {
	for want, change := range map[string]func(*Lab){
		"no targets":      func(l *Lab) { l.Targets = nil },
		"no computer":     func(l *Lab) { l.Targets[0].Computer = "mac" },
		"no KVM":          func(l *Lab) { l.Targets[0].KVM = "mini-c" },
		"KVM type":        func(l *Lab) { l.KVMs["mini-a"] = KVM{Config: kvm.Config{Type: "pikvm"}} },
		"has no usbip":    func(l *Lab) { l.Targets[0].Link = "usbip" },
		"link must be":    func(l *Lab) { l.Targets[0].Link = "wifi" },
		"is listed twice": func(l *Lab) { l.Targets = append(l.Targets, l.Targets[0]) },
	} {
		lab := testLab()
		change(&lab)
		if _, err := checkLab(lab, ""); err == nil || !strings.Contains(err.Error(), want) {
			t.Errorf("got %v, want an error with %q", err, want)
		}
	}
}
