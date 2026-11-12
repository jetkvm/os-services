package kvm

import (
	"context"
	"strings"
	"testing"
	"time"
)

func TestJetKVMWithoutPasswordConnectsWithoutLoginAndAnswersRPC(t *testing.T) {
	server, fake := newFake(t, &fakeKVM{})
	ctx, cancel := context.WithTimeout(context.Background(), 20*time.Second)
	defer cancel()
	// A password from JETKVM_PASSWORD reaches every KVM of the lab; this
	// KVM refuses a login.
	k, err := Open(ctx, Config{Type: TypeJetKVM, URL: server.URL, Password: "for-other-kvms"})
	if err != nil {
		t.Fatal(err)
	}
	defer k.Close()
	firmware, err := k.Firmware(ctx)
	if err != nil || firmware != "0.5.11" {
		t.Errorf("firmware %q, %v; want the application version 0.5.11", firmware, err)
	}
	state, err := k.State(ctx)
	if err != nil || !state.Ready || state.Width != 1920 {
		t.Errorf("state %+v, %v", state, err)
	}
	if _, err := k.Settings(ctx); err == nil || !strings.Contains(err.Error(), "Method not found") {
		t.Errorf("an RPC that the KVM does not have: got %v, want its error", err)
	}
	if timed, err := k.StartTimingLog(ctx); timed || err != nil {
		t.Errorf("a firmware without the timing log: got %v, %v; want none and no error", timed, err)
	}
	fake.mu.Lock()
	fake.timingLog = `{"running":true,"keyCount":1,"frameCount":1,"keys":[[1000,4]],"frames":[[1500,90000]]}`
	fake.mu.Unlock()
	if timed, err := k.StartTimingLog(ctx); !timed || err != nil {
		t.Errorf("a firmware with the timing log: got %v, %v; want a log and no error", timed, err)
	}
	if log, err := k.ReadTimingLog(ctx); err != nil || len(log.Keys) != 1 || log.Frames[90000] != 1500*time.Microsecond {
		t.Errorf("timing log: got %+v, %v", log, err)
	}
	fake.mu.Lock()
	defer fake.mu.Unlock()
	if fake.logins != 0 {
		t.Errorf("%d logins, want none", fake.logins)
	}
}

func TestJetKVMWithPasswordLogsInBeforeTheSignaling(t *testing.T) {
	server, fake := newFake(t, &fakeKVM{password: "secret"})
	ctx, cancel := context.WithTimeout(context.Background(), 20*time.Second)
	defer cancel()
	if _, err := Open(ctx, Config{Type: TypeJetKVM, URL: server.URL}); err == nil || !strings.Contains(err.Error(), "password") {
		t.Fatalf("no password: got %v, want an error that asks for the password", err)
	}
	k, err := Open(ctx, Config{Type: TypeJetKVM, URL: server.URL, Password: "secret"})
	if err != nil {
		t.Fatal(err)
	}
	defer k.Close()
	if _, err := k.State(ctx); err != nil {
		t.Error(err)
	}
	fake.mu.Lock()
	defer fake.mu.Unlock()
	if fake.logins != 1 {
		t.Errorf("%d logins, want 1", fake.logins)
	}
}
