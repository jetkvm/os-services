package kvm

import (
	"context"
	"encoding/base64"
	"encoding/binary"
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/coder/websocket"
	"github.com/coder/websocket/wsjson"
	"github.com/pion/webrtc/v4"
)

// fakeKVM is a KVM's local auth, WebSocket signaling and data channels with
// a pion peer, by the rules of the firmware of its type.
//
// Both types: with a password, the protected routes need the cookie of a
// login. The KVM answers a "ping" text with "pong". The fake sends its
// candidates before its answer, as the firmware can.
//
// A JetKVM without a password refuses a login.
//
// A Mini accepts the WebSocket only with the Origin of its own web page,
// takes only component-1 candidates, ends the WebRTC session with the
// WebSocket, and closes a WebSocket that is silent for idle.
type fakeKVM struct {
	t    *testing.T
	mini bool
	// password is empty for the noPassword mode.
	password string
	// idle is zero for a KVM that keeps a silent WebSocket.
	idle time.Duration

	mu sync.Mutex
	// timingLog is the answer of /webrtc/stats, empty for a firmware that
	// does not have the path.
	timingLog  string
	logins     int
	idleCloses int
	// received is the data of the file that a Mini took.
	received []byte
}

const fakeToken = "f3d1c0de"

func newFake(t *testing.T, f *fakeKVM) (*httptest.Server, *fakeKVM) {
	f.t = t
	server := httptest.NewServer(http.HandlerFunc(f.serve))
	t.Cleanup(server.Close)
	return server, f
}

func (f *fakeKVM) serve(w http.ResponseWriter, r *http.Request) {
	if r.URL.Path == "/auth/login-local" {
		var body struct{ Password string }
		json.NewDecoder(r.Body).Decode(&body)
		switch {
		case f.password == "":
			http.Error(w, `{"error":"Login is disabled in noPassword mode"}`, http.StatusBadRequest)
		case body.Password != f.password:
			http.Error(w, `{"error":"Invalid password"}`, http.StatusUnauthorized)
		default:
			f.mu.Lock()
			f.logins++
			f.mu.Unlock()
			http.SetCookie(w, &http.Cookie{Name: "authToken", Value: fakeToken, Path: "/"})
			w.Write([]byte(`{"message":"Login successful"}`))
		}
		return
	}
	if cookie, err := r.Cookie("authToken"); f.password != "" && (err != nil || cookie.Value != fakeToken) {
		http.Error(w, `{"error":"Unauthorized"}`, http.StatusUnauthorized)
		return
	}
	switch r.URL.Path {
	case "/device":
		w.Write([]byte(`{"authMode":"noPassword"}`))
	case "/webrtc/signaling/client":
		f.signal(w, r)
	case "/webrtc/stats":
		f.mu.Lock()
		log := f.timingLog
		f.mu.Unlock()
		if log == "" || !r.URL.Query().Has("timing") {
			w.WriteHeader(http.StatusNotFound)
			return
		}
		w.Write([]byte(log))
	default:
		// The firmware answers a GET of a path it does not have with its
		// web page, when the request accepts HTML.
		if accept := r.Header.Get("Accept"); accept != "" && !strings.Contains(accept, "text/html") && !strings.Contains(accept, "*/*") {
			w.WriteHeader(http.StatusNotFound)
			return
		}
		w.Header().Set("Content-Type", "text/html")
		w.Write([]byte("<!doctype html><title>JetKVM</title>"))
	}
}

func (f *fakeKVM) signal(w http.ResponseWriter, r *http.Request) {
	if f.mini && r.Header.Get("Origin") != "http://"+r.Host {
		http.Error(w, `{"error":"Device UI origin required"}`, http.StatusForbidden)
		return
	}
	conn, err := websocket.Accept(w, r, nil)
	if err != nil {
		f.t.Error(err)
		return
	}
	defer conn.CloseNow()
	conn.SetReadLimit(1 << 20)
	ctx := r.Context()
	wsjson.Write(ctx, conn, map[string]any{"type": "device-metadata", "data": map[string]any{"deviceVersion": "0.5.11"}})
	var pc *webrtc.PeerConnection
	defer func() {
		if pc != nil {
			pc.Close()
		}
	}()
	for {
		readCtx, cancel := ctx, context.CancelFunc(func() {})
		if f.idle > 0 {
			readCtx, cancel = context.WithTimeout(ctx, f.idle)
		}
		kind, data, err := conn.Read(readCtx)
		silent := readCtx.Err() == context.DeadlineExceeded
		cancel()
		if err != nil {
			if silent {
				f.mu.Lock()
				f.idleCloses++
				f.mu.Unlock()
			}
			return
		}
		if kind == websocket.MessageText && string(data) == "ping" {
			conn.Write(ctx, websocket.MessageText, []byte("pong"))
			continue
		}
		var message struct {
			Type string          `json:"type"`
			Data json.RawMessage `json:"data"`
		}
		if json.Unmarshal(data, &message) != nil {
			continue
		}
		switch message.Type {
		case "offer":
			var request struct {
				SD string `json:"sd"`
			}
			json.Unmarshal(message.Data, &request)
			raw, _ := base64.StdEncoding.DecodeString(request.SD)
			var offer webrtc.SessionDescription
			json.Unmarshal(raw, &offer)
			// A JetKVM sends H.265 when the offer has it.
			if strings.Contains(strings.ToUpper(offer.SDP), "H265") {
				f.t.Error("the offer has H.265")
			}
			if strings.Contains(offer.SDP, "a=candidate:") {
				f.t.Error("the offer carries candidates")
			}
			pc = f.answer(ctx, conn, offer)
		case "new-ice-candidate":
			var candidate webrtc.ICECandidateInit
			json.Unmarshal(message.Data, &candidate)
			if fields := strings.Fields(candidate.Candidate); f.mini && (len(fields) < 2 || fields[1] != "1") {
				f.t.Errorf("a candidate that a Mini refuses: %q", candidate.Candidate)
				continue
			}
			if pc != nil {
				pc.AddICECandidate(candidate)
			}
		}
	}
}

func (f *fakeKVM) answer(ctx context.Context, conn *websocket.Conn, offer webrtc.SessionDescription) *webrtc.PeerConnection {
	pc, err := webrtc.NewPeerConnection(webrtc.Configuration{})
	if err != nil {
		f.t.Error(err)
		return nil
	}
	track, _ := webrtc.NewTrackLocalStaticRTP(webrtc.RTPCodecCapability{MimeType: webrtc.MimeTypeH264}, "video", "kvm")
	pc.AddTrack(track)
	pc.OnDataChannel(func(dc *webrtc.DataChannel) {
		switch {
		case dc.Label() == "rpc":
			f.serveRPC(dc)
		case f.mini && dc.Label() == "guestfile_10":
			f.receiveFile(dc)
		}
	})
	if err := pc.SetRemoteDescription(offer); err != nil {
		f.t.Error(err)
		return pc
	}
	answer, _ := pc.CreateAnswer(nil)
	gathered := webrtc.GatheringCompletePromise(pc)
	pc.SetLocalDescription(answer)
	<-gathered
	sdp := pc.LocalDescription().SDP
	for _, candidate := range localCandidates(sdp) {
		wsjson.Write(ctx, conn, map[string]any{"type": "new-ice-candidate", "data": webrtc.ICECandidateInit{Candidate: candidate}})
	}
	description, _ := json.Marshal(webrtc.SessionDescription{Type: webrtc.SDPTypeAnswer, SDP: withoutCandidates(sdp)})
	wsjson.Write(ctx, conn, map[string]any{"type": "answer", "data": base64.StdEncoding.EncodeToString(description)})
	return pc
}

// serveRPC answers the JSON-RPC of the "rpc" data channel. A JetKVM sends
// videoInputState when the channel opens.
func (f *fakeKVM) serveRPC(dc *webrtc.DataChannel) {
	send := func(message map[string]any) {
		message["jsonrpc"] = "2.0"
		data, _ := json.Marshal(message)
		dc.SendText(string(data))
	}
	if !f.mini {
		dc.OnOpen(func() {
			send(map[string]any{"method": "videoInputState", "params": map[string]any{"ready": true, "width": 1920, "height": 1080}})
		})
	}
	dc.OnMessage(func(message webrtc.DataChannelMessage) {
		var request struct {
			ID     any    `json:"id"`
			Method string `json:"method"`
		}
		json.Unmarshal(message.Data, &request)
		if result := f.result(request.Method); result != nil {
			send(map[string]any{"id": request.ID, "result": result})
		} else {
			send(map[string]any{"id": request.ID, "error": map[string]any{"code": -32601, "message": "Method not found"}})
		}
	})
}

// result is the result of an RPC method, nil for a method that the KVM does
// not have.
func (f *fakeKVM) result(method string) any {
	switch {
	case f.mini && method == "guestFileDrop":
		return map[string]any{"channel": "guestfile_10", "transfers": []uint32{11}}
	case f.mini && method == "getVideoState":
		return map[string]any{"source": "guest", "osServices": map[string]any{
			"connected": true, "fingerprint": "AA:BB:CC:DD:EE:FF:00:11", "sender": map[string]any{"version": "0.4.7"},
		}}
	case f.mini:
		return nil
	case method == "getLocalVersion":
		return map[string]any{"appVersion": "0.5.11", "systemVersion": "0.2.7"}
	case method == "getVideoState":
		return map[string]any{"ready": true, "width": 1920, "height": 1080, "fps": 60}
	}
	return nil
}

// fileCredit is the credit that the fake gives at a time, less than one
// file so the sender must wait for more.
const fileCredit = 40 * 1024

// fileSize is the size of the file that receiveFile takes.
const fileSize = 100*1024 + 3

// receiveFile takes transfer 11 as the Mini does: credit first, then more
// credit as bytes arrive, then "done" with a path.
func (f *fakeKVM) receiveFile(dc *webrtc.DataChannel) {
	credit := func(n int) {
		message, _ := json.Marshal(map[string]any{"type": "credit", "transfer": 11, "bytes": n})
		dc.SendText(string(message))
	}
	dc.OnOpen(func() { credit(fileCredit) })
	given := fileCredit
	dc.OnMessage(func(message webrtc.DataChannelMessage) {
		if message.IsString || binary.LittleEndian.Uint32(message.Data) != 11 {
			f.t.Errorf("unexpected message on the file channel: %q", message.Data)
			return
		}
		f.mu.Lock()
		f.received = append(f.received, message.Data[4:]...)
		got := len(f.received)
		f.mu.Unlock()
		if got > given {
			f.t.Errorf("the sender sent %d bytes with %d credit", got, given)
		}
		if got == fileSize {
			done, _ := json.Marshal(map[string]any{"type": "done", "transfer": 11, "status": "ok", "path": "/home/tester/Downloads/x.bin"})
			dc.SendText(string(done))
		} else if got == given {
			given += fileCredit
			credit(fileCredit)
		}
	})
}
