package lab

import (
	"context"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"slices"
	"strings"
	"text/tabwriter"
	"time"

	"github.com/jetkvm/os-services/tools/e2e/build"
	"github.com/jetkvm/os-services/tools/e2e/computer"
	"github.com/jetkvm/os-services/tools/e2e/kvm"
)

// cleanupTimeout limits one step that puts back what a target changed.
const cleanupTimeout = time.Minute

// Select is the targets named in names, comma-separated, or all of them
// when names is empty.
func Select(all []Target, names string) ([]Target, error) {
	if names == "" {
		return all, nil
	}
	var selected []Target
	for _, name := range strings.Split(names, ",") {
		i := slices.IndexFunc(all, func(t Target) bool { return t.Name == name })
		if i < 0 {
			return nil, fmt.Errorf("no target %q in the lab file", name)
		}
		selected = append(selected, all[i])
	}
	return selected, nil
}

// Build builds the checkout that the tool runs from for each system that
// the targets run, and returns each system's installer.
func Build(ctx context.Context, lab Lab, targets []Target) (map[string]string, error) {
	top, err := exec.CommandContext(ctx, "git", "rev-parse", "--show-toplevel").Output()
	if err != nil {
		return nil, fmt.Errorf("the tool must run inside the checkout: %w", err)
	}
	root := strings.TrimSpace(string(top))
	needed := Systems(targets)
	configs := map[string]build.Config{}
	for _, system := range needed {
		config, ok := lab.Build[system]
		if !ok {
			return nil, fmt.Errorf("the lab file has no build entry for %s", system)
		}
		configs[system] = config
	}
	out := filepath.Join(root, "target", "e2e-build")
	fmt.Printf("building %s from %s (logs in %s)\n", strings.Join(needed, ", "), root, out)
	started := time.Now()
	return build.All(ctx, root, out, configs, func(format string, args ...any) {
		fmt.Printf("%s after %s\n", fmt.Sprintf(format, args...), time.Since(started).Round(time.Second))
	})
}

// Session is a target that is ready: its KVM is connected, the build is
// installed, and the service runs with an approved session.
type Session struct {
	KVM      *kvm.KVM
	Computer *computer.Computer
	Log      func(format string, args ...any)
}

// Result is the outcome of one target.
type Result struct {
	Name, Link, Firmware string
	// Failed is what failed, or "setup" when the run did not get to the
	// tool's work; empty when the target passed.
	Failed string
	Err    error
	// Skipped names the checks that the lab file left out for the computer.
	Skipped []string
}

// Run connects the USB, installs the installer, starts the desktop and the
// service, and gives the session to work, which returns what failed and
// why. Then it puts back what it changed: the service's state, the desktop,
// the KVM's settings and the USB.
func Run(ctx context.Context, t Target, installer string, work func(context.Context, Session) (string, error)) (r Result) {
	r = Result{Name: t.Name, Link: t.Link.String(), Failed: "setup"}
	logf := func(format string, args ...any) { fmt.Printf("[%s] %s\n", t.Name, fmt.Sprintf(format, args...)) }
	undo := func(what string, run func(context.Context) error) { putBack(ctx, logf, what, run) }
	if t.USBIP {
		restore, err := t.Computer.ReleaseOwnKVM(ctx)
		if r.Err = err; err != nil {
			return r
		}
		defer undo("switch the computer's own KVM on again", restore)
	}
	logf("connect USB: %s", t.Link)
	// Disconnect also undoes a Connect that failed halfway, for example a
	// KVM bound on the hub that the computer never attached.
	defer undo("disconnect USB", t.Link.Disconnect)
	if r.Err = t.Link.Connect(ctx); r.Err != nil {
		return r
	}
	k, err := kvm.Open(ctx, t.KVM)
	if r.Err = err; err != nil {
		return r
	}
	defer k.Close()
	r.Firmware, _ = k.Firmware(ctx)
	state, err := k.State(ctx)
	if r.Err = err; err != nil {
		return r
	}
	saved, err := k.Settings(ctx)
	if r.Err = err; err != nil {
		return r
	}
	defer undo("restore the KVM's settings", func(ctx context.Context) error { return k.SetSettings(ctx, saved) })
	logf("install %s with KVM %s approved", filepath.Base(installer), state.OSServices.Fingerprint)
	if r.Err = t.Computer.Install(ctx, installer, state.OSServices.Fingerprint); r.Err != nil {
		return r
	}
	// The desktop stops also after a start that failed halfway.
	stopDesktop, err := t.Computer.StartDesktop(ctx)
	defer undo("stop the desktop", stopDesktop)
	if r.Err = err; err != nil {
		return r
	}
	if r.Err = t.Computer.Start(ctx); r.Err != nil {
		return r
	}
	defer undo("stop the service", t.Computer.Stop)
	r.Failed, r.Err = work(ctx, Session{KVM: k, Computer: t.Computer, Log: logf})
	return r
}

// putBack runs one step that puts back what a target changed. It runs after
// an interrupt too, and its own timeout keeps a KVM or computer that stopped
// answering from holding up the steps after it.
func putBack(ctx context.Context, logf func(string, ...any), what string, run func(context.Context) error) {
	ctx, cancel := context.WithTimeout(context.WithoutCancel(ctx), cleanupTimeout)
	defer cancel()
	if err := run(ctx); err != nil {
		logf("%s: %v", what, err)
	}
}

// Report prints one line per target and reports whether all passed.
func Report(results []Result) bool {
	passed := true
	table := tabwriter.NewWriter(os.Stdout, 0, 4, 2, ' ', 0)
	fmt.Fprintln(table, "\nTARGET\tUSB\tFIRMWARE\tRESULT")
	for _, r := range results {
		outcome := "passed"
		if len(r.Skipped) > 0 {
			outcome = "passed without " + strings.Join(r.Skipped, ", ")
		}
		if r.Failed != "" {
			passed = false
			outcome = fmt.Sprintf("FAILED %s: %v", r.Failed, r.Err)
		}
		fmt.Fprintf(table, "%s\t%s\t%s\t%s\n", r.Name, r.Link, r.Firmware, outcome)
	}
	table.Flush()
	return passed
}
