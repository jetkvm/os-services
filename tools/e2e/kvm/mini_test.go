package kvm

import (
	"bytes"
	"context"
	"crypto/rand"
	"testing"
	"time"
)

func TestADescriptionGoesWithoutItsCandidates(t *testing.T) {
	sdp := "v=0\r\nm=video 9 UDP/TLS/RTP/SAVPF 102\r\na=candidate:1 1 udp 2130706431 192.0.2.20 5000 typ host\r\na=end-of-candidates\r\na=mid:0\r\n"
	if got, want := withoutCandidates(sdp), "v=0\r\nm=video 9 UDP/TLS/RTP/SAVPF 102\r\na=mid:0\r\n"; got != want {
		t.Errorf("got %q, want %q", got, want)
	}
}

func TestMiniTakesTheSignalingOfItsWebPageAndAnswersRPC(t *testing.T) {
	server, _ := newFake(t, &fakeKVM{mini: true})
	ctx, cancel := context.WithTimeout(context.Background(), 20*time.Second)
	defer cancel()
	k, err := Open(ctx, Config{URL: server.URL})
	if err != nil {
		t.Fatal(err)
	}
	state, err := k.State(ctx)
	if err != nil {
		t.Fatal(err)
	}
	if !state.Session() || state.OSServices.Sender.Version != "0.4.7" || state.Source != "guest" {
		t.Errorf("state: %+v", state)
	}
	if err := k.Close(); err != nil {
		t.Error(err)
	}
}

func TestASessionOutlastsTheIdleLimitOfTheMinisWebSocket(t *testing.T) {
	interval := pingInterval
	t.Cleanup(func() { pingInterval = interval })
	pingInterval = 50 * time.Millisecond
	const idle = 300 * time.Millisecond
	server, fake := newFake(t, &fakeKVM{mini: true, idle: idle})
	ctx, cancel := context.WithTimeout(context.Background(), 20*time.Second)
	defer cancel()
	k, err := Open(ctx, Config{URL: server.URL})
	if err != nil {
		t.Fatal(err)
	}
	defer k.Close()
	time.Sleep(3 * idle)
	stateCtx, stop := context.WithTimeout(ctx, 3*time.Second)
	defer stop()
	if _, err := k.State(stateCtx); err != nil {
		t.Errorf("the session after 3 idle limits: %v", err)
	}
	fake.mu.Lock()
	defer fake.mu.Unlock()
	if fake.idleCloses != 0 {
		t.Errorf("the Mini closed %d silent WebSockets, want none", fake.idleCloses)
	}
}

func TestDropFileSendsWithinTheCreditAndReturnsThePath(t *testing.T) {
	server, fake := newFake(t, &fakeKVM{mini: true})
	ctx, cancel := context.WithTimeout(context.Background(), 20*time.Second)
	defer cancel()
	k, err := Open(ctx, Config{URL: server.URL})
	if err != nil {
		t.Fatal(err)
	}
	defer k.Close()
	data := make([]byte, fileSize)
	rand.Read(data)
	path, err := k.DropFile(ctx, "x.bin", data)
	if err != nil {
		t.Fatal(err)
	}
	if path != "/home/tester/Downloads/x.bin" {
		t.Errorf("path %q", path)
	}
	fake.mu.Lock()
	defer fake.mu.Unlock()
	if !bytes.Equal(fake.received, data) {
		t.Errorf("received %d bytes that differ from the %d sent", len(fake.received), len(data))
	}
}

func TestARecordingStartsAtAnAccessUnitWithASequenceParameterSet(t *testing.T) {
	sps := []byte{0, 0, 0, 1, 0x67, 0x42, 0, 0, 0, 1, 0x68, 0xce, 0, 0, 0, 1, 0x65, 0x88}
	slice := []byte{0, 0, 0, 1, 0x41, 0x9a}
	if hasSPS(slice) || !hasSPS(sps) {
		t.Errorf("hasSPS: slice %t, keyframe %t", hasSPS(slice), hasSPS(sps))
	}
}
