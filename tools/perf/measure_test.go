package main

import (
	"math"
	"slices"
	"strings"
	"testing"
	"time"

	"github.com/jetkvm/os-services/tools/e2e/kvm"
	"github.com/jetkvm/os-services/tools/e2e/probe"
)

// Stage lines as the service writes them: an idle one, then three with
// frames.
const serviceLog = `hardware H.265 encoder available: hevc_amf
stage window=5.0s fresh=0 cursor=0 idle=0 capped=0 late=0 kf=0 fps=0.0 kbps=0 cap[-] conv[-] enc[-] usb[-] gap[-]
stage window=5.0s fresh=37 cursor=5 idle=288 capped=0 late=0 kf=2 fps=8.4 kbps=1367 cap[p50=11.43 p95=15.97 max=20.90] conv[p50=1.04 p95=1.49 max=1.94] enc[p50=14.85 p95=15.55 max=21.86] usb[p50=1.28 p95=2.90 max=3.49] gap[p50=16.75 p95=730.51 max=1111.03]
stage window=5.0s fresh=40 cursor=2 idle=280 capped=0 late=0 kf=1 fps=8.0 kbps=1300 cap[p50=12.00 p95=17.00 max=19.00] conv[p50=1.10 p95=1.60 max=2.00] enc[p50=15.00 p95=16.00 max=17.00] usb[p50=1.30 p95=2.00 max=2.10] gap[p50=17.00 p95=700.00 max=900.00]
stage window=5.0s fresh=38 cursor=4 idle=290 capped=0 late=0 kf=1 fps=7.6 kbps=1250 cap[p50=11.80 p95=16.00 max=18.00] conv[p50=1.00 p95=1.50 max=1.80] enc[p50=14.90 p95=15.20 max=16.00] usb[p50=1.20 p95=2.40 max=2.50] gap[p50=16.90 p95=710.00 max=800.00]
`

func TestServiceStatsTakeTheMedianP50AndTheLargestP95(t *testing.T) {
	stats, err := serviceStats(serviceLog, 3)
	if err != nil {
		t.Fatal(err)
	}
	if got := stats.stages["enc"]; got != (Stage{P50: 14.90, P95: 16.00}) {
		t.Errorf("enc: got %+v, want p50 14.90 and p95 16.00", got)
	}
	if got := stats.stages["cap"]; got != (Stage{P50: 11.80, P95: 17.00}) {
		t.Errorf("cap: got %+v, want p50 11.80 and p95 17.00", got)
	}
	if got, want := stats.captureFPS, (37+40+38)/15.0; got != want {
		t.Errorf("capture rate: got %.2f, want %.2f", got, want)
	}
}

func TestServiceStatsOfTwoLinesTakeTheP50BetweenThem(t *testing.T) {
	stats, err := serviceStats(serviceLog, 2)
	if err != nil {
		t.Fatal(err)
	}
	// The last two lines have enc p50 15.00 and 14.90, and cap p50 12.00
	// and 11.80.
	if got := stats.stages["enc"].P50; math.Abs(got-14.95) > 1e-9 {
		t.Errorf("enc p50: got %.3f, want 14.950", got)
	}
	if got := stats.stages["cap"].P50; math.Abs(got-11.90) > 1e-9 {
		t.Errorf("cap p50: got %.3f, want 11.900", got)
	}
}

func TestPercentilesDoNotReportALatencyBelowThePercentile(t *testing.T) {
	ms := func(values ...int) []time.Duration {
		var latencies []time.Duration
		for _, value := range values {
			latencies = append(latencies, time.Duration(value)*time.Millisecond)
		}
		return latencies
	}
	for want, latencies := range map[string][]time.Duration{
		"p50 30.0 ms, p95 30.0 ms (1 of 60 keys)":   ms(30),
		"p50 10.0 ms, p95 20.0 ms (2 of 60 keys)":   ms(20, 10),
		"p50 50.0 ms, p95 100.0 ms (10 of 60 keys)": ms(10, 20, 30, 40, 50, 60, 70, 80, 90, 100),
		"p50 100.0 ms, p95 190.0 ms (20 of 60 keys)": ms(10, 20, 30, 40, 50, 60, 70, 80, 90, 100,
			110, 120, 130, 140, 150, 160, 170, 180, 190, 200),
	} {
		if got := percentiles(latencies); got != want {
			t.Errorf("%d keys: got %q, want %q", len(latencies), got, want)
		}
	}
}

func TestKeyLinesFailWhenNoKeyShowed(t *testing.T) {
	lines, err := keyLines(keyTimes{noKVM: "no key showed", focusLosses: 2})
	if err == nil || !strings.Contains(err.Error(), "no key of 60 showed") || !strings.Contains(err.Error(), "lost the focus 2 times") {
		t.Errorf("no key: got lines %q and the error %v, want a no-key error with the focus losses", lines, err)
	}
	// A KVM whose firmware has no timing log still gives the time to the
	// video.
	video := []time.Duration{40 * time.Millisecond}
	lines, err = keyLines(keyTimes{video: video, noKVM: "the firmware of the KVM has no timing log"})
	if err != nil || len(lines) != 2 || !strings.Contains(lines[0], "key to video p50 40.0 ms") || !strings.Contains(lines[1], "not measured") {
		t.Errorf("no timing log: got lines %q and the error %v", lines, err)
	}
}

func TestServiceStatsFailWithoutFramesInTheWindow(t *testing.T) {
	if _, err := serviceStats(serviceLog, 4); err == nil || !strings.Contains(err.Error(), "no frame") {
		t.Errorf("a window with an idle line: got %v, want a no-frame error", err)
	}
	if _, err := serviceStats(serviceLog, 5); err == nil || !strings.Contains(err.Error(), "4 stage lines") {
		t.Errorf("five lines from a log with four: got %v, want an error that counts four", err)
	}
}

func TestBlockValueReadsTheBlocksLeftToRight(t *testing.T) {
	stride := blockCrop * probe.Blocks
	luma := make([]byte, stride*blockCrop)
	// Blocks 1 and 4 white: 01001 is 9, the letter i.
	for y := range blockCrop {
		for _, block := range []int{1, 4} {
			for x := range blockCrop {
				luma[y*stride+block*blockCrop+x] = 235
			}
		}
	}
	if got := blockValue(luma); got != 9 {
		t.Errorf("got %d, want 9", got)
	}
}

func TestKeyLatenciesTimeTheFirstFrameThatShowsTheLetter(t *testing.T) {
	start := time.Now()
	at := func(ms int) time.Time { return start.Add(time.Duration(ms) * time.Millisecond) }
	// Keys a (1) at 0 ms, b (2) at 100 ms, a (1) again at 200 ms.
	sent := []time.Time{at(0), at(100), at(200)}
	want := []int{1, 2, 1}
	values := []int{
		0, // 10 ms: nothing typed yet
		1, // 40 ms: a shows
		1, // 120 ms: b has not shown yet
		2, // 140 ms: b shows
		2, // 230 ms: the second a has not shown yet
		1, // 260 ms: the second a shows
	}
	arrivals := []time.Time{at(10), at(40), at(120), at(140), at(230), at(260)}
	got := keyLatencies(values, arrivals, sent, want)
	wantLatencies := []time.Duration{40 * time.Millisecond, 40 * time.Millisecond, 60 * time.Millisecond}
	if !slices.Equal(got, wantLatencies) {
		t.Errorf("got %v, want %v", got, wantLatencies)
	}
}

func TestKeyLatenciesLeaveOutALetterThatAlreadyShows(t *testing.T) {
	start := time.Now()
	at := func(ms int) time.Time { return start.Add(time.Duration(ms) * time.Millisecond) }
	// b never shows, so when the second a is pressed the window still shows
	// the first a, and no frame can time the second one.
	sent := []time.Time{at(0), at(100), at(200)}
	want := []int{1, 2, 1}
	values := []int{0, 1, 1, 1}
	arrivals := []time.Time{at(10), at(40), at(150), at(250)}
	got := keyLatencies(values, arrivals, sent, want)
	if want := []time.Duration{40 * time.Millisecond}; !slices.Equal(got, want) {
		t.Errorf("got %v, want %v", got, want)
	}
}

// timingLog is a log of the KVM, 5 s after its start: the keys a, b and a
// with their releases, and six frames, 1,530 RTP ticks apart.
func timingLog() (kvm.TimingLog, []uint32) {
	at := func(ms int) time.Duration { return 5*time.Second + time.Duration(ms)*time.Millisecond }
	log := kvm.TimingLog{KeyCount: 6, FrameCount: 6, Frames: map[uint32]time.Duration{}, Keys: []kvm.KeyTime{
		{Code: keyA, Written: at(0)}, {Code: 0, Written: at(1)},
		{Code: keyA + 1, Written: at(100)}, {Code: 0, Written: at(101)},
		{Code: keyA, Written: at(200)}, {Code: 0, Written: at(201)},
	}}
	var timestamps []uint32
	for i, ms := range []int{10, 40, 120, 140, 230, 260} {
		timestamp := uint32(450000 + 1530*i)
		timestamps = append(timestamps, timestamp)
		log.Frames[timestamp] = at(ms)
	}
	return log, timestamps
}

func TestKVMKeyLatenciesAreTimesOfTheKVMsClock(t *testing.T) {
	log, timestamps := timingLog()
	// The frames show nothing, a, a, b, b, a.
	got, err := kvmKeyLatencies(log, []int{0, 1, 1, 2, 2, 1}, timestamps, []int{1, 2, 1})
	if err != nil {
		t.Fatal(err)
	}
	want := []time.Duration{40 * time.Millisecond, 40 * time.Millisecond, 60 * time.Millisecond}
	if !slices.Equal(got, want) {
		t.Errorf("got %v, want %v", got, want)
	}
}

func TestKVMKeyLatenciesFailWhenTheLogIsNotWhole(t *testing.T) {
	values, want := []int{0, 1, 1, 2, 2, 1}, []int{1, 2, 1}
	for name, c := range map[string]struct {
		change func(log *kvm.TimingLog)
		err    string
	}{
		"a frame is not in the log":     {func(log *kvm.TimingLog) { delete(log.Frames, 450000+1530*3) }, "no frame"},
		"a key press is not in the log": {func(log *kvm.TimingLog) { log.Keys = log.Keys[:4] }, "2 of 3 key presses"},
		"a different key is in the log": {func(log *kvm.TimingLog) { log.Keys[2].Code = keyA + 7 }, "key 2"},
	} {
		log, timestamps := timingLog()
		c.change(&log)
		if _, err := kvmKeyLatencies(log, values, timestamps, want); err == nil || !strings.Contains(err.Error(), c.err) {
			t.Errorf("%s: got %v, want an error with %q", name, err, c.err)
		}
	}
}
