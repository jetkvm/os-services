package kvm

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"net/http"
	"time"
)

// TimingLog is the KVM's log of two events, each with its time on the KVM's
// own clock: a keyboard report left on USB, and the last byte of a video
// frame was there. The time from a key to a frame is then the difference of
// two times of one clock, and the network to the KVM is not part of it.
type TimingLog struct {
	// Keys are the keyboard reports in the order the KVM wrote them.
	Keys []KeyTime
	// Frames is the time of each frame by the RTP timestamp of its packets.
	Frames map[uint32]time.Duration
	// KeyCount and FrameCount count every entry since the start. The KVM
	// keeps only the newest, so a count above the number of entries tells
	// that the oldest are gone.
	KeyCount, FrameCount int
}

// KeyTime is one keyboard report: the code of its first key, 0 when no key
// is down, and when the KVM wrote it.
type KeyTime struct {
	Code    byte
	Written time.Duration
}

// parseTimingLog reads the KVM's answer. A Mini whose firmware has no timing
// log answers with its statistics, which have no "running": then ok is false.
func parseTimingLog(answer []byte) (log TimingLog, ok bool, err error) {
	var wire struct {
		Running    *bool      `json:"running"`
		KeyCount   int        `json:"keyCount"`
		FrameCount int        `json:"frameCount"`
		Keys       [][2]int64 `json:"keys"`   // written µs, key code
		Frames     [][2]int64 `json:"frames"` // complete µs, RTP timestamp
	}
	if err := json.Unmarshal(answer, &wire); err != nil {
		return TimingLog{}, false, fmt.Errorf("the KVM's timing log: %w", err)
	}
	if wire.Running == nil {
		return TimingLog{}, false, nil
	}
	log = TimingLog{Frames: map[uint32]time.Duration{}, KeyCount: wire.KeyCount, FrameCount: wire.FrameCount}
	for _, key := range wire.Keys {
		log.Keys = append(log.Keys, KeyTime{Code: byte(key[1]), Written: time.Duration(key[0]) * time.Microsecond})
	}
	for _, frame := range wire.Frames {
		log.Frames[uint32(frame[1])] = time.Duration(frame[0]) * time.Microsecond
	}
	return log, true, nil
}

// timingLog asks the KVM for its timing log. A JetKVM whose firmware has no
// timing log does not have the path, and answers 404 Not Found.
func (k *KVM) timingLog(ctx context.Context, action string) (TimingLog, bool, error) {
	answer, err := k.web.raw(ctx, http.MethodGet, "/webrtc/stats?timing="+action, nil)
	var status *statusError
	if errors.As(err, &status) && status.code == http.StatusNotFound {
		return TimingLog{}, false, nil
	}
	if err != nil {
		return TimingLog{}, false, err
	}
	return parseTimingLog(answer)
}

// StartTimingLog starts an empty timing log on the KVM. It reports false
// when the KVM's firmware has no timing log.
func (k *KVM) StartTimingLog(ctx context.Context) (bool, error) {
	_, ok, err := k.timingLog(ctx, "start")
	return ok, err
}

// ReadTimingLog returns the timing log since StartTimingLog.
func (k *KVM) ReadTimingLog(ctx context.Context) (TimingLog, error) {
	log, _, err := k.timingLog(ctx, "read")
	return log, err
}

// StopTimingLog stops the timing log, and the KVM frees its memory.
func (k *KVM) StopTimingLog(ctx context.Context) error {
	_, _, err := k.timingLog(ctx, "stop")
	return err
}
