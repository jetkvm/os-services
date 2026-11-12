package main

import (
	"context"
	"fmt"
	"maps"
	"math"
	"math/rand/v2"
	"regexp"
	"slices"
	"strconv"
	"strings"
	"time"

	"github.com/jetkvm/os-services/tools/e2e/kvm"
	"github.com/jetkvm/os-services/tools/e2e/lab"
	"github.com/jetkvm/os-services/tools/e2e/probe"
)

const (
	// perfResolution is the KVM's OS Services resolution for every perf
	// measurement, so that the targets compare.
	perfResolution = "1920x1080"
	// perfWarmup lets the stream settle before the measurement.
	perfWarmup = 5 * time.Second
	// perfWindow is the throughput measurement. It holds two whole stage
	// lines of the service, which writes one each 5 s.
	perfWindow = 15 * time.Second
	// telemetryInterval is how often the service writes a stage line.
	telemetryInterval = 5 * time.Second
)

// The key latency: the KVM types letters about 100 ms apart into the
// key window, and each letter is timed until the first frame whose blocks
// show it. A letter comes back after 26 others, 2.6 s later, so a frame
// that shows it belongs to the latest one.
const (
	keys      = 60
	keyGap    = 90 * time.Millisecond
	keyJitter = 20 * time.Millisecond
	// blockCrop is the side, in pixels, of the square read from the middle
	// of each block; a block is about 55 pixels wide at 1920x1080.
	blockCrop = 16
	// keyA is the HID usage code of the letter A; the other letters follow.
	keyA = 0x04
)

// Stage is one pipeline stage of the service, in milliseconds.
type Stage struct {
	P50 float64
	P95 float64
}

// measure measures the video pipeline of a session at perfResolution and
// fails only when it cannot measure. For perfWindow, the noise window
// changes the screen at each refresh, so every system works under the same
// load, and measure reports the frame rate and bitrate at the KVM, and the
// frames the service captured and its stage times from its log. Then, on a
// still screen, the KVM types letters into the key window, and measure
// reports the time from each key press to the frame that shows the letter,
// two times: on the clock of this computer, with the network to the KVM in
// it, and on the clock of the KVM, without. No clocks of two computers are
// compared.
func measure(ctx context.Context, s lab.Session) error {
	state, err := s.KVM.GuestVideo(ctx, perfResolution)
	if err != nil {
		return err
	}
	stopNoise, err := s.Computer.StartNoiseWindow(ctx)
	if err != nil {
		return err
	}
	time.Sleep(perfWarmup)
	report, err := s.KVM.Video(ctx, perfWindow)
	log := ""
	if err == nil {
		// The noise window still runs, so the last stage lines of the log
		// are whole lines of the load.
		log, err = s.Computer.Log(ctx)
	}
	stopNoise(context.WithoutCancel(ctx))
	if err != nil {
		return err
	}
	if got := fmt.Sprintf("%dx%d", report.Width, report.Height); got != perfResolution {
		return fmt.Errorf("the video is %s, not %s", got, perfResolution)
	}
	service, err := serviceStats(log, int(perfWindow/telemetryInterval)-1)
	if err != nil {
		return err
	}
	// The recording starts at its first keyframe, which arrives some time
	// into perfWindow.
	seconds := report.Duration.Seconds()
	line := fmt.Sprintf("%dx%d %.1f fps %.0f kbit/s, captured %.1f fps",
		report.Width, report.Height, float64(report.Frames)/seconds, float64(report.Bytes)*8/1000/seconds, service.captureFPS)
	for _, name := range slices.Sorted(maps.Keys(service.stages)) {
		stage := service.stages[name]
		line += fmt.Sprintf(", %s %.2f/%.2f ms", name, stage.P50, stage.P95)
	}
	line += lostPackets(report.Lost, report.Packets)
	s.Log("%s", line)
	times, err := keyLatency(ctx, s, state.Width, state.Height)
	if err != nil {
		return err
	}
	lines, err := keyLines(times)
	for _, line := range lines {
		s.Log("%s", line)
	}
	return err
}

// keyLines are the result lines of the key measurement. With no key that
// showed in the video there is no time to report, and keyLines fails.
func keyLines(times keyTimes) ([]string, error) {
	focus := ""
	if times.focusLosses > 0 {
		focus = fmt.Sprintf(", the key window lost the focus %d times", times.focusLosses)
	}
	focus += lostPackets(times.lost, times.packets)
	if len(times.video) == 0 {
		return nil, fmt.Errorf("no key of %d showed in the video%s", keys, focus)
	}
	lines := []string{fmt.Sprintf("key to video %s%s", percentiles(times.video), focus)}
	if times.noKVM != "" {
		return append(lines, "key to frame at the KVM: not measured: "+times.noKVM), nil
	}
	return append(lines, "key to frame at the KVM "+percentiles(times.atKVM)), nil
}

// percentiles is the p50 and the p95 of the latencies of the keys that
// showed, which are one or more. A percentile is the smallest latency
// that the given part of the keys did not exceed.
func percentiles(latencies []time.Duration) string {
	slices.Sort(latencies)
	percentile := func(p float64) float64 {
		return float64(latencies[int(math.Ceil(p*float64(len(latencies))))-1].Microseconds()) / 1000
	}
	return fmt.Sprintf("p50 %.1f ms, p95 %.1f ms (%d of %d keys)", percentile(0.5), percentile(0.95), len(latencies), keys)
}

// keyTimes is the result of the key measurement.
type keyTimes struct {
	// video is, for each letter that showed, the time from its key press
	// here to the arrival here of the first frame that shows it.
	video []time.Duration
	// atKVM is the same on the KVM's clock: from the key report on USB to
	// the last byte of that frame at the KVM. noKVM tells why there is none.
	atKVM []time.Duration
	noKVM string
	// focusLosses is how often the key window lost the keyboard focus.
	focusLosses int
	// lost is how many packets of the recording were lost for good, and
	// packets how many arrived.
	lost, packets int
}

// lostPackets is the part of a line that tells how many packets were lost
// for good, out of how many: the network between the KVM and this computer
// is part of every number.
func lostPackets(lost, arrived int) string {
	if lost == 0 {
		return ""
	}
	return fmt.Sprintf(", lost %d of %d packets (%.1f%%)", lost, lost+arrived, 100*float64(lost)/float64(lost+arrived))
}

// keyLatency types the letters into the key window on a screen of width x
// height and times each letter that showed before the same letter came
// again.
func keyLatency(ctx context.Context, s lab.Session, width, height int) (keyTimes, error) {
	stopWindow, err := s.Computer.StartKeyWindow(ctx)
	if err != nil {
		return keyTimes{}, err
	}
	defer stopWindow(context.WithoutCancel(ctx))
	// A click gives the window the focus where a window manager or the
	// system does not, as a Wayland compositor does for no window that the
	// user did not click. The kernel reports an absolute position only when
	// it changes, and a desktop that started since the KVM's last report
	// keeps its pointer where it put it, so the pointer first goes to the
	// middle of the screen.
	if err := s.KVM.MovePointer(ctx, 16384, 16384); err != nil {
		return keyTimes{}, err
	}
	time.Sleep(50 * time.Millisecond)
	// The window reports its place again when a window manager moves it
	// after it showed, so the place is read after that pause.
	place, err := s.Computer.KeyWindowPlace(ctx)
	if err != nil {
		return keyTimes{}, err
	}
	// The KVM's absolute pointer covers the whole desktop, and the video
	// shows the KVM's screen.
	fx, fy := probe.Focus(place.Desktop)
	if err := s.KVM.Click(ctx, int(fx*32767), int(fy*32767)); err != nil {
		return keyTimes{}, err
	}
	time.Sleep(500 * time.Millisecond)
	// The KVM's timing log runs from before the recording until after it,
	// so it has each key and each frame of the recording.
	noKVM := ""
	if timed, err := s.KVM.StartTimingLog(ctx); err != nil {
		noKVM = err.Error()
	} else if !timed {
		noKVM = "the firmware of the KVM has no timing log"
	} else {
		defer func() {
			// A KVM that does not answer must not hold the cleanup after it.
			stop, cancel := context.WithTimeout(context.WithoutCancel(ctx), 5*time.Second)
			defer cancel()
			_ = s.KVM.StopTimingLog(stop)
		}()
	}
	// The recording starts at the keyframe it asks for; the keys start a
	// second later and leave a second at the end.
	duration := 2*time.Second + keys*(keyGap+keyJitter)
	type recorded struct {
		kvm.Recording
		err error
	}
	done := make(chan recorded, 1)
	go func() {
		r, err := s.KVM.Record(ctx, duration)
		done <- recorded{r, err}
	}()
	time.Sleep(time.Second)
	sent := make([]time.Time, keys)
	want := make([]int, keys)
	for i := range sent {
		time.Sleep(keyGap + rand.N(keyJitter))
		want[i] = i%26 + 1
		sent[i] = time.Now()
		if err := s.KVM.PressKey(ctx, byte(keyA+i%26)); err != nil {
			return keyTimes{}, err
		}
	}
	r := <-done
	if r.err != nil {
		return keyTimes{}, r.err
	}
	var log kvm.TimingLog
	if noKVM == "" {
		if log, err = s.KVM.ReadTimingLog(ctx); err != nil {
			noKVM = err.Error()
		}
	}
	var corners [][2]int
	for i := range probe.Blocks {
		bx, by := probe.BlockCenter(i)
		x := (place.Screen.Left + bx*place.Screen.Width) * float64(width)
		y := (place.Screen.Top + by*place.Screen.Height) * float64(height)
		corners = append(corners, [2]int{int(x) - blockCrop/2, int(y) - blockCrop/2})
	}
	crops, err := kvm.Crops(ctx, r.Recording, blockCrop, corners)
	if err != nil {
		return keyTimes{}, err
	}
	if len(crops) == 0 {
		return keyTimes{}, fmt.Errorf("no frame decoded from %d access units", len(r.Units))
	}
	values := make([]int, len(crops))
	arrivals := make([]time.Time, len(crops))
	timestamps := make([]uint32, len(crops))
	for i, c := range crops {
		values[i], arrivals[i], timestamps[i] = blockValue(c.Luma), r.Arrivals[c.Unit], r.Timestamps[c.Unit]
	}
	losses, err := s.Computer.KeyFocusLosses(ctx)
	if err != nil {
		return keyTimes{}, err
	}
	times := keyTimes{video: keyLatencies(values, arrivals, sent, want), noKVM: noKVM,
		focusLosses: losses, lost: r.Lost, packets: r.Packets}
	if noKVM == "" {
		if times.atKVM, err = kvmKeyLatencies(log, values, timestamps, want); err != nil {
			times.noKVM = err.Error()
		} else if len(times.atKVM) == 0 {
			times.noKVM = "no key showed"
		}
	}
	return times, nil
}

// kvmKeyLatencies is keyLatencies on the clock of the KVM: for each key, the
// time from when the KVM wrote its report to USB to when the KVM had the last
// byte of the first frame that shows it. timestamps are the RTP timestamps of
// the frames of values. It fails when the log does not have each key and each
// frame.
func kvmKeyLatencies(log kvm.TimingLog, values []int, timestamps []uint32, want []int) ([]time.Duration, error) {
	// A report with no key down is the release of the key before it.
	var written []time.Time
	for _, key := range log.Keys {
		if key.Code == 0 {
			continue
		}
		if i := len(written); i < len(want) && int(key.Code) != keyA+want[i]-1 {
			return nil, fmt.Errorf("key %d in the timing log of the KVM has the code %d, not %d", i+1, key.Code, keyA+want[i]-1)
		}
		written = append(written, time.UnixMicro(key.Written.Microseconds()))
	}
	if len(written) != len(want) {
		return nil, fmt.Errorf("the timing log of the KVM has %d of %d key presses, in %d of %d reports",
			len(written), len(want), len(log.Keys), log.KeyCount)
	}
	complete := make([]time.Time, len(timestamps))
	for i, timestamp := range timestamps {
		at, ok := log.Frames[timestamp]
		if !ok {
			return nil, fmt.Errorf("the timing log of the KVM has no frame with the RTP timestamp %d; it has %d of %d frames",
				timestamp, len(log.Frames), log.FrameCount)
		}
		complete[i] = time.UnixMicro(at.Microseconds())
	}
	return keyLatencies(values, complete, written, want), nil
}

// blockValue reads the value that the blocks of one frame show from their
// crops side by side, as kvm.Crops returns them: a block is white when its
// mean luma is above the middle.
func blockValue(luma []byte) int {
	value := 0
	stride := blockCrop * probe.Blocks
	for i := range probe.Blocks {
		sum := 0
		for y := range blockCrop {
			for x := range blockCrop {
				sum += int(luma[y*stride+i*blockCrop+x])
			}
		}
		value <<= 1
		if sum > 128*blockCrop*blockCrop {
			value |= 1
		}
	}
	return value
}

// keyLatencies finds, for key i sent at sent[i] for value want[i], the first
// frame that arrived after it and shows that value, before the same value is
// sent again. A key is left out when the last frame before it already shows
// its value, because the letter before it never showed.
func keyLatencies(values []int, arrivals, sent []time.Time, want []int) []time.Duration {
	var latencies []time.Duration
	for i, at := range sent {
		before := -1
		for j := range values {
			if arrivals[j].After(at) {
				break
			}
			before = j
		}
		if before >= 0 && values[before] == want[i] {
			continue
		}
		end := time.Time{}
		for k := i + 1; k < len(sent); k++ {
			if want[k] == want[i] {
				end = sent[k]
				break
			}
		}
		for j, value := range values {
			if !arrivals[j].After(at) || value != want[i] {
				continue
			}
			if end.IsZero() || arrivals[j].Before(end) {
				latencies = append(latencies, arrivals[j].Sub(at))
			}
			break
		}
	}
	return latencies
}

// stageLine matches one stage time of a telemetry line, for example
// "enc[p50=11.03 p95=11.93 max=45.04]", and freshFrames its count of new
// frames.
var (
	stageLine   = regexp.MustCompile(`(\w+)\[p50=([\d.]+) p95=([\d.]+)`)
	freshFrames = regexp.MustCompile(` fresh=(\d+) `)
)

// service is what the service's log says about the measurement window.
type service struct {
	stages     map[string]Stage
	captureFPS float64
}

// serviceStats reads the last count stage lines of the service's log, which
// the service wrote under the load of the noise window: for each stage the
// median of its p50s and the largest of its p95s, and the new frames a
// second. A line without
// stage times had no frame to time, so the workload did not reach the
// capture.
func serviceStats(log string, count int) (service, error) {
	var lines []string
	for _, line := range strings.Split(log, "\n") {
		if strings.HasPrefix(line, "stage ") {
			lines = append(lines, line)
		}
	}
	if len(lines) < count {
		return service{}, fmt.Errorf("%d stage lines in the service's log, need %d", len(lines), count)
	}
	p50s, p95s := map[string][]float64{}, map[string]float64{}
	fresh := 0
	for _, line := range lines[len(lines)-count:] {
		matches := stageLine.FindAllStringSubmatch(line, -1)
		frames := freshFrames.FindStringSubmatch(line)
		if len(matches) == 0 || frames == nil {
			return service{}, fmt.Errorf("no frame in a %s window of the measurement: %s", telemetryInterval, line)
		}
		n, _ := strconv.Atoi(frames[1])
		fresh += n
		for _, match := range matches {
			p50, _ := strconv.ParseFloat(match[2], 64)
			p95, _ := strconv.ParseFloat(match[3], 64)
			p50s[match[1]] = append(p50s[match[1]], p50)
			p95s[match[1]] = max(p95s[match[1]], p95)
		}
	}
	stats := service{stages: map[string]Stage{}, captureFPS: float64(fresh) / (float64(count) * telemetryInterval.Seconds())}
	for name, values := range p50s {
		slices.Sort(values)
		middle := len(values) / 2
		median := values[middle]
		if len(values)%2 == 0 {
			median = (values[middle-1] + median) / 2
		}
		stats.stages[name] = Stage{P50: median, P95: p95s[name]}
	}
	return stats, nil
}
