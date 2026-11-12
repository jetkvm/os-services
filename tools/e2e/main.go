// Command e2e tests one build of OS Services on several computers through a
// real KVM, and prints one result line per target: compatibility, pass or
// fail. Command perf measures the same build's video pipeline.
//
//	go run ./e2e [-targets a,b] [-checks session,files] lab.json
//
// It builds the checkout it runs from, uncommitted changes included, as the
// lab file's build entries say, and tests that build. See README.md.
package main

import (
	"context"
	"errors"
	"flag"
	"fmt"
	"os"
	"os/signal"
	"slices"
	"strings"
	"time"

	"github.com/jetkvm/os-services/tools/e2e/check"
	"github.com/jetkvm/os-services/tools/e2e/lab"
	"github.com/jetkvm/os-services/tools/e2e/ssh"
)

// checkTimeout limits one check.
const checkTimeout = 3 * time.Minute

func main() {
	only := flag.String("targets", "", "run only these targets (computer/kvm), comma-separated")
	checks := flag.String("checks", "", "run only these checks, comma-separated (session always runs)")
	printKey := flag.Bool("lab-key", false, "create the lab's SSH key if needed, print its public key file and exit")
	flag.Usage = func() {
		fmt.Fprintln(os.Stderr, "usage: e2e [-targets a,b] [-checks session,files] lab.json")
		fmt.Fprintln(os.Stderr, "       e2e -lab-key")
		flag.PrintDefaults()
	}
	flag.Parse()
	if *printKey {
		key, err := ssh.LabPublicKey()
		if err != nil {
			fail(err)
		}
		fmt.Println(key)
		return
	}
	if flag.NArg() != 1 {
		flag.Usage()
		os.Exit(2)
	}
	setup, targets, err := lab.Load(flag.Arg(0))
	if err == nil {
		targets, err = lab.Select(targets, *only)
	}
	selected, checkErr := selectChecks(*checks)
	if err = errors.Join(err, checkErr); err != nil {
		fail(err)
	}
	for _, t := range targets {
		if _, err := withoutSkipped(selected, t.Computer.Skip()); err != nil {
			fail(fmt.Errorf("%s: %w", t.Name, err))
		}
	}
	ctx, stop := signal.NotifyContext(context.Background(), os.Interrupt)
	defer stop()
	installers, err := lab.Build(ctx, setup, targets)
	if err != nil {
		fail(err)
	}
	var results []lab.Result
	for _, t := range targets {
		run, _ := withoutSkipped(selected, t.Computer.Skip())
		result := lab.Run(ctx, t, installers[t.OS], func(ctx context.Context, s lab.Session) (string, error) {
			return runChecks(ctx, s, run)
		})
		for _, c := range selected {
			if !slices.ContainsFunc(run, func(r check.Check) bool { return r.Name == c.Name }) {
				result.Skipped = append(result.Skipped, c.Name)
			}
		}
		results = append(results, result)
	}
	if !lab.Report(results) {
		os.Exit(1)
	}
}

func fail(err error) {
	fmt.Fprintln(os.Stderr, "e2e:", err)
	os.Exit(2)
}

func selectChecks(names string) ([]check.Check, error) {
	if names == "" {
		return check.All, nil
	}
	wanted := strings.Split(names, ",")
	selected := []check.Check{check.All[0]}
	for _, name := range wanted {
		if !slices.ContainsFunc(check.All, func(c check.Check) bool { return c.Name == name }) {
			return nil, fmt.Errorf("no check %q", name)
		}
	}
	for _, c := range check.All[1:] {
		if slices.Contains(wanted, c.Name) {
			selected = append(selected, c)
		}
	}
	return selected, nil
}

// withoutSkipped is checks without the ones that the lab file leaves out
// for a computer. The session check stays: the others need its session.
func withoutSkipped(checks []check.Check, skip []string) ([]check.Check, error) {
	for _, name := range skip {
		if name == check.All[0].Name || !slices.ContainsFunc(check.All, func(c check.Check) bool { return c.Name == name }) {
			return nil, fmt.Errorf("skip: %q is no check that a computer can leave out", name)
		}
	}
	var run []check.Check
	for _, c := range checks {
		if !slices.Contains(skip, c.Name) {
			run = append(run, c)
		}
	}
	return run, nil
}

// runChecks runs the checks in order and returns the first that failed.
func runChecks(ctx context.Context, s lab.Session, checks []check.Check) (string, error) {
	for _, c := range checks {
		checkCtx, cancel := context.WithTimeout(ctx, checkTimeout)
		err := c.Run(checkCtx, s)
		cancel()
		if err != nil {
			s.Log("%s: FAILED: %v", c.Name, err)
			return c.Name, err
		}
		s.Log("%s: ok", c.Name)
	}
	return "", nil
}
