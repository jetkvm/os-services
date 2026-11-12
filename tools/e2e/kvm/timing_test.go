package kvm

import (
	"testing"
	"time"
)

func TestParseTimingLogReadsTheKVMsAnswer(t *testing.T) {
	// The answer as the Mini's host test of the log prints it.
	answer := `{"running":true,"keyCount":2,"frameCount":3,` +
		`"keys":[[1000,4],[2000,0]],` +
		`"frames":[[1500,90000],[18166,91530],[34833,93060]]}`
	log, ok, err := parseTimingLog([]byte(answer))
	if err != nil || !ok {
		t.Fatalf("got ok %v and error %v", ok, err)
	}
	if len(log.Keys) != 2 || log.Keys[0] != (KeyTime{Code: 4, Written: time.Millisecond}) || log.Keys[1].Code != 0 {
		t.Errorf("keys: got %+v", log.Keys)
	}
	if got := log.Frames[91530]; got != 18166*time.Microsecond {
		t.Errorf("the frame with RTP timestamp 91530: got %v, want 18.166 ms", got)
	}
	if len(log.Frames) != 3 || log.KeyCount != 2 || log.FrameCount != 3 {
		t.Errorf("got %d frames and the counts %d and %d", len(log.Frames), log.KeyCount, log.FrameCount)
	}
}

func TestParseTimingLogTellsAKVMWithoutALog(t *testing.T) {
	// A firmware without the log ignores the query and sends its statistics.
	_, ok, err := parseTimingLog([]byte(`{"started":true,"modeValid":true,"fps":60}`))
	if err != nil || ok {
		t.Errorf("got ok %v and error %v, want no log and no error", ok, err)
	}
}
