package lab

import (
	"encoding/json"
	"fmt"
	"os"
	"sort"

	"github.com/jetkvm/os-services/tools/e2e/build"
	"github.com/jetkvm/os-services/tools/e2e/computer"
	"github.com/jetkvm/os-services/tools/e2e/kvm"
	"github.com/jetkvm/os-services/tools/e2e/link"
)

// Lab is a lab file: how to build each system, the KVMs and computers of
// the lab, and the targets, each one computer with one KVM.
type Lab struct {
	// Build is how e2e builds each system.
	Build     map[string]build.Config    `json:"build"`
	KVMs      map[string]KVM             `json:"kvms"`
	Computers map[string]computer.Config `json:"computers"`
	Targets   []Entry                    `json:"targets"`
}

// KVM is a KVM of the lab: a JetKVM Mini or a JetKVM, as its type says.
// USBIP is set for a KVM whose USB a hub shares.
type KVM struct {
	kvm.Config
	USBIP *link.Config `json:"usbip,omitempty"`
}

// Entry is one target of a lab file: a computer with a KVM. Link is
// "usbip" for the KVM over the network; without it the KVM is on a cable.
type Entry struct {
	Computer string `json:"computer"`
	KVM      string `json:"kvm"`
	Link     string `json:"link,omitempty"`
}

// Target is a checked Entry, ready to run.
type Target struct {
	Name     string
	OS       string
	KVM      kvm.Config
	Computer *computer.Computer
	Link     link.Link
	USBIP    bool
}

// Load reads and checks a lab file. A KVM without a password in the
// file takes JETKVM_PASSWORD.
func Load(file string) (Lab, []Target, error) {
	data, err := os.ReadFile(file)
	if err != nil {
		return Lab{}, nil, err
	}
	var lab Lab
	if err := json.Unmarshal(data, &lab); err != nil {
		return Lab{}, nil, fmt.Errorf("%s: %w", file, err)
	}
	targets, err := checkLab(lab, os.Getenv("JETKVM_PASSWORD"))
	return lab, targets, err
}

func checkLab(lab Lab, password string) ([]Target, error) {
	if len(lab.Targets) == 0 {
		return nil, fmt.Errorf("the lab file lists no targets")
	}
	seen := map[string]bool{}
	var checked []Target
	for _, t := range lab.Targets {
		name := t.Computer + "/" + t.KVM
		if seen[name] {
			return nil, fmt.Errorf("target %s is listed twice", name)
		}
		seen[name] = true
		computerConfig, ok := lab.Computers[t.Computer]
		if !ok {
			return nil, fmt.Errorf("%s: no computer %q in computers", name, t.Computer)
		}
		kvmEntry, ok := lab.KVMs[t.KVM]
		if !ok {
			return nil, fmt.Errorf("%s: no KVM %q in kvms", name, t.KVM)
		}
		if err := kvmEntry.CheckType(); err != nil {
			return nil, fmt.Errorf("%s: %w", name, err)
		}
		c, err := computer.New(computerConfig)
		if err != nil {
			return nil, fmt.Errorf("%s: %w", name, err)
		}
		var l link.Link = link.Cable{}
		switch t.Link {
		case "":
		case "usbip":
			if kvmEntry.USBIP == nil {
				return nil, fmt.Errorf("%s: KVM %q has no usbip entry", name, t.KVM)
			}
			if l, err = link.NewUSBIP(*kvmEntry.USBIP, computerConfig.SSH); err != nil {
				return nil, fmt.Errorf("%s: %w", name, err)
			}
		default:
			return nil, fmt.Errorf("%s: link must be empty (cable) or \"usbip\", not %q", name, t.Link)
		}
		kvmConfig := kvmEntry.Config
		if kvmConfig.Password == "" {
			kvmConfig.Password = password
		}
		checked = append(checked, Target{Name: name, OS: computerConfig.OS, KVM: kvmConfig, Computer: c, Link: l, USBIP: t.Link == "usbip"})
	}
	return checked, nil
}

// Systems are the operating systems of targets, in order.
func Systems(targets []Target) []string {
	set := map[string]bool{}
	for _, t := range targets {
		set[t.OS] = true
	}
	var list []string
	for system := range set {
		list = append(list, system)
	}
	sort.Strings(list)
	return list
}
