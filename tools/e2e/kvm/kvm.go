// Package kvm is the device side of an end-to-end run: a JetKVM Mini or a
// JetKVM that a test controls as the KVM's web page does.
package kvm

import (
	"context"
	"encoding/json"
	"fmt"
	"strings"
	"sync"
	"sync/atomic"
	"time"

	"github.com/pion/webrtc/v4"
)

// The types of KVM that Config.Type names.
const (
	// TypeMini is a JetKVM Mini. It is the default.
	TypeMini = "mini"
	// TypeJetKVM is a JetKVM.
	TypeJetKVM = "jetkvm"
)

// State is the part of the KVM's video state that the checks read.
type State struct {
	Source     string     `json:"source"`
	Ready      bool       `json:"ready"`
	Width      int        `json:"width"`
	Height     int        `json:"height"`
	OSServices OSServices `json:"osServices"`
	GuestFile  struct {
		Receive bool `json:"receive"`
	} `json:"guestFile"`
}

// OSServices is the KVM's view of the service on the computer.
type OSServices struct {
	Connected       bool   `json:"connected"`
	ApprovalPending bool   `json:"approvalPending"`
	Fingerprint     string `json:"fingerprint"`
	Video           bool   `json:"video"`
	VideoError      string `json:"videoError"`
	Sender          *struct {
		Version string `json:"version"`
	} `json:"sender"`
}

// Session reports whether the service has an approved session open.
func (s State) Session() bool {
	return s.OSServices.Connected && !s.OSServices.ApprovalPending && s.OSServices.Sender != nil
}

// VideoReport is what a decoder made of recorded video.
type VideoReport struct {
	Frames        int
	Width, Height int
	// Bytes is the size of the recorded H.264 stream.
	Bytes int
	// Errors are the decoder's error lines.
	Errors []string
	// Packets is the number of RTP packets that arrived, and Lost the
	// number that never arrived, also not as a retransmission.
	Packets, Lost int
	// Duration is the time from the arrival of the first frame to the end
	// of the recording.
	Duration time.Duration
}

// Config names a KVM.
type Config struct {
	// Type is TypeMini or TypeJetKVM. Empty is TypeMini.
	Type string `json:"type,omitempty"`
	// URL is the KVM's web address, for example http://192.0.2.113.
	URL string `json:"url"`
	// Password is the KVM's web password, if it has one.
	Password string `json:"password,omitempty"`
}

// CheckType returns an error when Type names no type of KVM.
func (c Config) CheckType() error {
	switch c.Type {
	case "", TypeMini, TypeJetKVM:
		return nil
	}
	return fmt.Errorf("KVM type %q: use %q or %q", c.Type, TypeMini, TypeJetKVM)
}

// connectTimeout limits the signaling and the WebRTC connection.
const connectTimeout = 30 * time.Second

// KVM is one connected KVM: the signaling of the KVM's web page, then
// JSON-RPC on the "rpc" data channel, as the web page uses it. Every method
// waits at most as long as its context allows.
type KVM struct {
	kind   string
	signal *signaling
	pc     *webrtc.PeerConnection
	rpcDC  *webrtc.DataChannel
	video  *videoTap
	// web is the KVM's HTTP API with the session's login cookie.
	web *web

	nextID    atomic.Uint64
	pendingMu sync.Mutex
	pending   map[uint64]chan rpcAnswer
}

type rpcAnswer struct {
	ID     uint64          `json:"id"`
	Result json.RawMessage `json:"result"`
	Error  *struct {
		Message string          `json:"message"`
		Data    json.RawMessage `json:"data"`
	} `json:"error"`
}

// Open connects to the KVM that config names.
func Open(ctx context.Context, config Config) (_ *KVM, err error) {
	if err := config.CheckType(); err != nil {
		return nil, err
	}
	w, err := newWeb(config.URL)
	if err != nil {
		return nil, err
	}
	connectCtx, cancel := context.WithTimeout(ctx, connectTimeout)
	defer cancel()
	kind := config.Type
	if kind == "" {
		kind = TypeMini
	}
	signal, err := openSignaling(connectCtx, w, config.Password)
	if err != nil {
		return nil, err
	}
	pc, err := newPeer(w.base)
	if err != nil {
		signal.hangUp()
		return nil, err
	}
	k := &KVM{
		kind: kind, signal: signal, pc: pc, video: &videoTap{}, web: w,
		pending: map[uint64]chan rpcAnswer{},
	}
	defer func() {
		if err != nil {
			_ = k.Close()
		}
	}()
	connected, rpcOpen, err := k.configurePeer()
	if err != nil {
		return nil, err
	}
	if err := signal.negotiate(connectCtx, pc); err != nil {
		return nil, err
	}
	for _, ready := range []<-chan struct{}{connected, rpcOpen} {
		select {
		case <-ready:
		case <-connectCtx.Done():
			return nil, fmt.Errorf("connect to %s: %w", config.URL, connectCtx.Err())
		}
	}
	return k, nil
}

func (k *KVM) configurePeer() (connected, rpcOpen chan struct{}, err error) {
	connected, rpcOpen = make(chan struct{}), make(chan struct{})
	var connectedOnce, rpcOnce sync.Once
	k.pc.OnConnectionStateChange(func(state webrtc.PeerConnectionState) {
		if state == webrtc.PeerConnectionStateConnected {
			connectedOnce.Do(func() { close(connected) })
		}
	})
	k.pc.OnTrack(func(track *webrtc.TrackRemote, _ *webrtc.RTPReceiver) {
		if strings.EqualFold(track.Codec().MimeType, webrtc.MimeTypeH264) {
			go k.video.read(k.pc, track)
		}
	})
	ordered := true
	k.rpcDC, err = k.pc.CreateDataChannel("rpc", &webrtc.DataChannelInit{Ordered: &ordered})
	if err != nil {
		return nil, nil, err
	}
	k.rpcDC.OnOpen(func() {
		rpcOnce.Do(func() {
			if raw, err := k.rpcDC.Detach(); err == nil {
				go readMessages(raw, func(data []byte, _ bool) { k.receive(data) })
			}
			close(rpcOpen)
		})
	})
	return connected, rpcOpen, nil
}

func (k *KVM) rpc(ctx context.Context, method string, params, result any) error {
	id := k.nextID.Add(1)
	answers := make(chan rpcAnswer, 1)
	k.pendingMu.Lock()
	k.pending[id] = answers
	k.pendingMu.Unlock()
	defer func() {
		k.pendingMu.Lock()
		delete(k.pending, id)
		k.pendingMu.Unlock()
	}()
	if params == nil {
		params = map[string]any{}
	}
	request, err := json.Marshal(map[string]any{"jsonrpc": "2.0", "id": id, "method": method, "params": params})
	if err != nil {
		return err
	}
	if err := k.rpcDC.SendText(string(request)); err != nil {
		return err
	}
	select {
	case answer := <-answers:
		if answer.Error != nil {
			return fmt.Errorf("%s: %s %s", method, answer.Error.Message, answer.Error.Data)
		}
		if result == nil {
			return nil
		}
		return json.Unmarshal(answer.Result, result)
	case <-ctx.Done():
		return fmt.Errorf("%s: %w", method, ctx.Err())
	}
}

// receive hands an answer to the call that waits for it. No call waits for
// an event, a message with a method and no ID.
func (k *KVM) receive(data []byte) {
	var answer rpcAnswer
	if json.Unmarshal(data, &answer) != nil {
		return
	}
	k.pendingMu.Lock()
	answers := k.pending[answer.ID]
	k.pendingMu.Unlock()
	if answer.ID != 0 && answers != nil {
		answers <- answer
	}
}

// Firmware is the version of the firmware that the KVM runs: the system
// version of a Mini, and the application version of a JetKVM.
func (k *KVM) Firmware(ctx context.Context) (string, error) {
	var version struct {
		AppVersion    string `json:"appVersion"`
		SystemVersion string `json:"systemVersion"`
	}
	err := k.rpc(ctx, "getLocalVersion", nil, &version)
	if k.kind == TypeJetKVM {
		return version.AppVersion, err
	}
	return version.SystemVersion, err
}

// State is the KVM's video and OS Services state.
func (k *KVM) State(ctx context.Context) (State, error) {
	var state State
	err := k.rpc(ctx, "getVideoState", nil, &state)
	return state, err
}

// Settings are the KVM's OS Services settings, as the KVM names them.
func (k *KVM) Settings(ctx context.Context) (map[string]any, error) {
	var settings map[string]any
	err := k.rpc(ctx, "getOsServicesSettings", nil, &settings)
	return settings, err
}

// SetSettings changes the named OS Services settings and keeps the rest.
func (k *KVM) SetSettings(ctx context.Context, settings map[string]any) error {
	return k.rpc(ctx, "setOsServicesSettings", map[string]any{"settings": settings}, nil)
}

// SetClipboard writes text to the computer's clipboard.
func (k *KVM) SetClipboard(ctx context.Context, text string) error {
	var answer struct {
		Status string `json:"status"`
	}
	if err := k.rpc(ctx, "guestClipboardSet", map[string]any{"text": text}, &answer); err != nil {
		return err
	}
	if answer.Status != "ok" {
		return fmt.Errorf("guestClipboardSet: status %q", answer.Status)
	}
	return nil
}

// Clipboard reads the computer's clipboard.
func (k *KVM) Clipboard(ctx context.Context) (string, error) {
	var answer struct {
		Status    string `json:"status"`
		Text      string `json:"text"`
		Truncated bool   `json:"truncated"`
	}
	if err := k.rpc(ctx, "guestClipboardGet", nil, &answer); err != nil {
		return "", err
	}
	if answer.Status != "ok" || answer.Truncated {
		return "", fmt.Errorf("guestClipboardGet: status %q, truncated %t", answer.Status, answer.Truncated)
	}
	return answer.Text, nil
}

// ClipboardStatus reads the computer's clipboard and returns the status of
// the answer, such as "ok" or "denied", and its text.
func (k *KVM) ClipboardStatus(ctx context.Context) (string, string, error) {
	var answer struct {
		Status string `json:"status"`
		Text   string `json:"text"`
	}
	err := k.rpc(ctx, "guestClipboardGet", nil, &answer)
	return answer.Status, answer.Text, err
}

// MovePointer moves the absolute pointer to x, y, each 0 to 32767.
func (k *KVM) MovePointer(ctx context.Context, x, y int) error {
	return k.rpc(ctx, "absMouseReport", map[string]any{"x": x, "y": y, "buttons": 0}, nil)
}

// Click moves the absolute pointer to x, y, each 0 to 32767, and clicks the
// left button there.
func (k *KVM) Click(ctx context.Context, x, y int) error {
	for _, buttons := range []int{0, 1, 0} {
		if err := k.rpc(ctx, "absMouseReport", map[string]any{"x": x, "y": y, "buttons": buttons}, nil); err != nil {
			return err
		}
		time.Sleep(50 * time.Millisecond)
	}
	return nil
}

// PressKey presses and releases the key with HID usage code, for example
// 0x04 for A.
func (k *KVM) PressKey(ctx context.Context, code byte) error {
	// keys is a list of numbers; a []byte would go out as a base64 string.
	for _, keys := range [][]int{{int(code), 0, 0, 0, 0, 0}, {0, 0, 0, 0, 0, 0}} {
		if err := k.rpc(ctx, "keyboardReport", map[string]any{"keys": keys, "modifier": 0}, nil); err != nil {
			return err
		}
	}
	return nil
}

// Close ends the signaling session and the WebRTC connection.
func (k *KVM) Close() error {
	k.signal.hangUp()
	return k.pc.Close()
}
