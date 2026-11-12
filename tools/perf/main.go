// Command perf measures one build of OS Services on the cable targets of a
// lab file, one at a time, at 1920x1080, and prints the numbers: the frame
// rate, bitrate and stage times under a noise window that changes the
// screen at each refresh, and the time from a key press to the frame that
// shows it. It fails a target only when it cannot measure. Command e2e
// tests compatibility with the same lab file.
//
//	go run ./perf [-targets a,b] lab.json
//
// It builds the checkout it runs from, uncommitted changes included. See
// README.md.
package main

import (
	"context"
	"flag"
	"fmt"
	"os"
	"os/signal"
	"time"

	"github.com/jetkvm/os-services/tools/e2e/lab"
)

// measureTimeout limits the measurement of one target.
const measureTimeout = 5 * time.Minute

func main() {
	only := flag.String("targets", "", "measure only these targets (computer/kvm), comma-separated")
	flag.Usage = func() {
		fmt.Fprintln(os.Stderr, "usage: perf [-targets a,b] lab.json")
		flag.PrintDefaults()
	}
	flag.Parse()
	if flag.NArg() != 1 {
		flag.Usage()
		os.Exit(2)
	}
	setup, targets, err := lab.Load(flag.Arg(0))
	if err == nil {
		targets, err = lab.Select(targets, *only)
	}
	if err != nil {
		fail(err)
	}
	// Over USB/IP the network and the hub would be part of every number,
	// so perf measures only targets with a cable.
	var cabled []lab.Target
	for _, t := range targets {
		if t.USBIP {
			fmt.Printf("[%s] skipped: perf measures only targets with a cable\n", t.Name)
			continue
		}
		cabled = append(cabled, t)
	}
	if len(cabled) == 0 {
		fail(fmt.Errorf("no target with a cable"))
	}
	ctx, stop := signal.NotifyContext(context.Background(), os.Interrupt)
	defer stop()
	installers, err := lab.Build(ctx, setup, cabled)
	if err != nil {
		fail(err)
	}
	var results []lab.Result
	for _, t := range cabled {
		results = append(results, lab.Run(ctx, t, installers[t.OS], func(ctx context.Context, s lab.Session) (string, error) {
			ctx, cancel := context.WithTimeout(ctx, measureTimeout)
			defer cancel()
			if err := measure(ctx, s); err != nil {
				s.Log("FAILED: %v", err)
				return "measure", err
			}
			return "", nil
		}))
	}
	if !lab.Report(results) {
		os.Exit(1)
	}
}

func fail(err error) {
	fmt.Fprintln(os.Stderr, "perf:", err)
	os.Exit(2)
}
