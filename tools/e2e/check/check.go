// Package check holds the end-to-end checks. Each one gets a KVM with an
// approved session to the service on the computer and returns an error when
// the service does not do its part.
package check

import (
	"context"
	"crypto/rand"
	"crypto/sha256"
	"encoding/hex"
	"fmt"
	"strings"
	"time"

	"github.com/jetkvm/os-services/tools/e2e/kvm"
	"github.com/jetkvm/os-services/tools/e2e/lab"
)

// Check is one named check.
type Check struct {
	Name string
	Run  func(ctx context.Context, env lab.Session) error
}

// All are the checks in the order they run. Session comes first: the others
// need its session.
var All = []Check{
	{Name: "session", Run: Session},
	{Name: "clipboard", Run: Clipboard},
	{Name: "clipboard-secret", Run: ClipboardSecret},
	{Name: "files", Run: Files},
	{Name: "video", Run: Video},
	{Name: "forget", Run: Forget},
	{Name: "reconnect", Run: Reconnect},
}

// Session: the service opens an approved session and reports its version.
func Session(ctx context.Context, env lab.Session) error {
	state, err := env.KVM.Until(ctx, 60*time.Second, "an approved session", kvm.State.Session)
	if err != nil {
		return err
	}
	env.Log("service %s connected", state.OSServices.Sender.Version)
	return nil
}

// Clipboard: text that the KVM writes to the computer's clipboard reads
// back the same, also text outside ASCII.
func Clipboard(ctx context.Context, env lab.Session) error {
	if err := env.KVM.SetSettings(ctx, map[string]any{"clipboard_enabled": true}); err != nil {
		return err
	}
	for _, text := range []string{
		"os-services e2e " + time.Now().Format(time.RFC3339Nano),
		"Grüße, 日本語, emoji 🙂, tab\tand\nnewline",
	} {
		// The service connects its clipboard after the session opens, once
		// the desktop's clipboard answers, and the KVM reports no state for
		// it, so a write waits for it as the files check waits for file
		// receive.
		deadline := time.Now().Add(30 * time.Second)
		for {
			err := env.KVM.SetClipboard(ctx, text)
			if err == nil {
				break
			}
			if !strings.Contains(err.Error(), "no OS Services clipboard is connected") || time.Now().After(deadline) {
				return err
			}
			select {
			case <-ctx.Done():
				return err
			case <-time.After(500 * time.Millisecond):
			}
		}
		got, err := env.KVM.Clipboard(ctx)
		if err != nil {
			return err
		}
		if strings.ReplaceAll(got, "\r\n", "\n") != text {
			return fmt.Errorf("clipboard: wrote %q, read %q", text, got)
		}
	}
	return nil
}

// ClipboardSecret: text that a password manager marks as secret stays on
// the computer. The service answers the KVM's read with "denied" and no
// text.
func ClipboardSecret(ctx context.Context, env lab.Session) error {
	if err := env.KVM.SetSettings(ctx, map[string]any{"clipboard_enabled": true}); err != nil {
		return err
	}
	stop, err := env.Computer.StartSecretClipboard(ctx)
	if err != nil {
		return err
	}
	defer stop(context.WithoutCancel(ctx))
	// The service's clipboard may connect after the session opens, as in
	// the clipboard check.
	deadline := time.Now().Add(30 * time.Second)
	for {
		status, text, err := env.KVM.ClipboardStatus(ctx)
		if err == nil {
			if text != "" {
				return fmt.Errorf("the KVM read %q with status %q", text, status)
			}
			if status != "denied" {
				return fmt.Errorf("status %q, want denied", status)
			}
			return nil
		}
		if !strings.Contains(err.Error(), "no OS Services clipboard is connected") || time.Now().After(deadline) {
			return err
		}
		select {
		case <-ctx.Done():
			return err
		case <-time.After(500 * time.Millisecond):
		}
	}
}

// Files: a dropped file arrives with the same bytes.
func Files(ctx context.Context, env lab.Session) error {
	if err := env.KVM.SetSettings(ctx, map[string]any{"files_enabled": true}); err != nil {
		return err
	}
	if _, err := env.KVM.Until(ctx, 30*time.Second, "file receive", func(s kvm.State) bool { return s.GuestFile.Receive }); err != nil {
		return err
	}
	data := make([]byte, 3*1024*1024+17)
	if _, err := rand.Read(data); err != nil {
		return err
	}
	name := fmt.Sprintf("os-services-e2e-%d.bin", time.Now().UnixNano())
	path, err := env.KVM.DropFile(ctx, name, data)
	if err != nil {
		return err
	}
	defer env.Computer.Remove(context.WithoutCancel(ctx), path)
	got, err := env.Computer.SHA256(ctx, path)
	if err != nil {
		return err
	}
	sum := sha256.Sum256(data)
	if want := hex.EncodeToString(sum[:]); got != want {
		return fmt.Errorf("files: %s has SHA-256 %s, sent %s", path, got, want)
	}
	env.Log("%d bytes arrived at %s", len(data), path)
	return nil
}

// videoWindow is how long the video check records.
const videoWindow = 8 * time.Second

// Video: the computer's screen arrives as video that decodes at the size
// that the KVM reports, and the KVM still shows the service's video after
// the recording.
func Video(ctx context.Context, env lab.Session) error {
	state, err := env.KVM.GuestVideo(ctx, "")
	if err != nil {
		return err
	}
	report, err := env.KVM.Video(ctx, videoWindow)
	if err != nil {
		return err
	}
	// The KVM shows its own capture again when the service's video stops,
	// and that video can have the same size.
	after, err := env.KVM.State(ctx)
	if err != nil {
		return err
	}
	switch {
	case after.Source != "guest" || !after.OSServices.Video:
		return fmt.Errorf("video: after the recording the KVM shows the source %q, not the service's video; its video error is %q", after.Source, after.OSServices.VideoError)
	case len(report.Errors) > 0:
		return fmt.Errorf("video: %d decoding errors: %s", len(report.Errors), strings.Join(report.Errors[:min(3, len(report.Errors))], " | "))
	case report.Frames == 0:
		return fmt.Errorf("video: no frame decoded in %s", videoWindow)
	case report.Width != state.Width || report.Height != state.Height:
		return fmt.Errorf("video: decoded %dx%d, the KVM reports %dx%d", report.Width, report.Height, state.Width, state.Height)
	}
	env.Log("%d frames at %dx%d in %s", report.Frames, report.Width, report.Height, videoWindow)
	return nil
}

// forgetLimit is how long the service has to end the session of a KVM that
// the user removed from the approved list. The service reads the list every
// second.
const forgetLimit = 5 * time.Second

// Forget: after the user removes the KVM from the approved list, the
// running service ends the session and opens no new one. After the user
// approves the KVM again, the session comes back.
func Forget(ctx context.Context, env lab.Session) error {
	state, err := env.KVM.State(ctx)
	if err != nil {
		return err
	}
	fingerprint := state.OSServices.Fingerprint
	if err := env.Computer.Forget(ctx, fingerprint); err != nil {
		return err
	}
	forgotten := time.Now()
	_, err = env.KVM.Until(ctx, forgetLimit, "the session to end", func(s kvm.State) bool { return !s.Session() })
	ended := time.Since(forgotten)
	if err == nil {
		err = staysForgotten(ctx, env.KVM)
	}
	// The KVM is approved again also after a failure or an interrupt: the
	// checks that follow, and the next run, need the session. The limit is
	// for a computer that stops answering.
	approveCtx, cancel := context.WithTimeout(context.WithoutCancel(ctx), time.Minute)
	defer cancel()
	if approveErr := env.Computer.Approve(approveCtx, fingerprint); err == nil {
		err = approveErr
	}
	if err != nil {
		return err
	}
	env.Log("the session ended within %s of --forget-kvm", ended.Round(100*time.Millisecond))
	_, err = env.KVM.Until(ctx, 60*time.Second, "the session to come back", kvm.State.Session)
	return err
}

// staysForgotten watches the KVM for longer than the service waits before
// it looks for the KVM again: no session may open with a forgotten KVM.
func staysForgotten(ctx context.Context, k *kvm.KVM) error {
	deadline := time.Now().Add(5 * time.Second)
	for time.Now().Before(deadline) {
		state, err := k.State(ctx)
		if err != nil {
			return err
		}
		if state.Session() {
			return fmt.Errorf("forget: a session opened with the forgotten KVM (state: %+v)", state)
		}
		select {
		case <-ctx.Done():
			return ctx.Err()
		case <-time.After(250 * time.Millisecond):
		}
	}
	return nil
}

// Reconnect: after the service stops and starts again, the session comes
// back.
func Reconnect(ctx context.Context, env lab.Session) error {
	if err := env.Computer.Stop(ctx); err != nil {
		return err
	}
	if _, err := env.KVM.Until(ctx, 30*time.Second, "the session to end", func(s kvm.State) bool { return !s.OSServices.Connected }); err != nil {
		return err
	}
	if err := env.Computer.Start(ctx); err != nil {
		return err
	}
	_, err := env.KVM.Until(ctx, 60*time.Second, "the session to come back", kvm.State.Session)
	return err
}
