package kvm

import (
	"context"
	"encoding/base64"
	"encoding/json"
	"errors"
	"fmt"
	"net/http"
	"sync"
	"time"

	"github.com/coder/websocket"
	"github.com/coder/websocket/wsjson"
	"github.com/pion/webrtc/v4"
)

// signaling is the signaling of the KVM's web page: a WebSocket at
// /webrtc/signaling/client. The KVM first sends device-metadata. Then the
// client offers, the KVM answers, and both send ICE candidates as
// new-ice-candidate. A description travels as the base64 of its JSON.
//
// A Mini keeps the WebRTC session only while the WebSocket is open, and
// closes a WebSocket that is silent for 90 seconds. So the WebSocket stays
// open until hangUp, and the client sends a "ping" text at the interval of
// the web page.
type signaling struct {
	conn *websocket.Conn
	// stop ends read, and closed closes when read ends, after it set
	// readErr.
	stop    context.CancelFunc
	closed  chan struct{}
	readErr error

	metadata     chan struct{}
	metadataOnce sync.Once
	answers      chan json.RawMessage
	refusals     chan json.RawMessage

	// pc is set once it has the KVM's answer; the KVM's candidates wait in
	// queued until then.
	mu     sync.Mutex
	pc     *webrtc.PeerConnection
	queued []webrtc.ICECandidateInit
}

// wsMessage is a message of the signaling WebSocket. The KVM sends Error
// when it cannot use an offer.
type wsMessage struct {
	Type  string          `json:"type"`
	Data  json.RawMessage `json:"data"`
	Error json.RawMessage `json:"error"`
}

// pingInterval is the time between two "ping" texts. A test of the idle
// rule sets it.
var pingInterval = 25 * time.Second

// openSignaling logs in when the KVM asks for a password and opens the
// signaling WebSocket. A KVM without a password answers GET /device without
// a cookie, and a JetKVM without a password refuses a login. A login gives
// a JetKVM a new token, which signs out the browsers that use the old one,
// so the client logs in only when the KVM asks for it.
//
// A Mini accepts the WebSocket only from its own web page: the Origin of
// the request must be the URL of the KVM.
func openSignaling(ctx context.Context, w *web, password string) (*signaling, error) {
	err := w.call(ctx, http.MethodGet, "/device", nil, nil)
	if unauthorized(err) {
		if password == "" {
			return nil, errors.New("the KVM has a password: give it in the lab file or in JETKVM_PASSWORD")
		}
		err = w.login(ctx, password)
	}
	if err != nil {
		return nil, err
	}
	conn, _, err := websocket.Dial(ctx, w.base+"/webrtc/signaling/client", &websocket.DialOptions{
		HTTPClient: w.client,
		HTTPHeader: http.Header{"Origin": {w.base}},
	})
	if err != nil {
		return nil, err
	}
	// The answer carries the KVM's whole SDP.
	conn.SetReadLimit(1 << 20)
	readCtx, stop := context.WithCancel(context.Background())
	s := &signaling{
		conn: conn, stop: stop, closed: make(chan struct{}),
		metadata: make(chan struct{}), answers: make(chan json.RawMessage, 1), refusals: make(chan json.RawMessage, 1),
	}
	go s.read(readCtx)
	go s.ping(readCtx)
	return s, nil
}

// ping sends a "ping" text at each interval until ctx ends or the KVM
// closes the WebSocket.
func (s *signaling) ping(ctx context.Context) {
	ticker := time.NewTicker(pingInterval)
	defer ticker.Stop()
	for {
		select {
		case <-ticker.C:
			if s.conn.Write(ctx, websocket.MessageText, []byte("ping")) != nil {
				return
			}
		case <-s.closed:
			return
		case <-ctx.Done():
			return
		}
	}
}

// read reads the WebSocket until ctx ends or the KVM closes it. It must
// read for the whole session: the WebSocket answers the KVM's pings only
// while it reads.
func (s *signaling) read(ctx context.Context) {
	defer close(s.closed)
	for {
		kind, data, err := s.conn.Read(ctx)
		if err != nil {
			s.readErr = err
			return
		}
		var message wsMessage
		// The KVM answers a "ping" text with a "pong" text, which is not
		// JSON.
		if kind != websocket.MessageText || json.Unmarshal(data, &message) != nil {
			continue
		}
		switch {
		case message.Error != nil:
			select {
			case s.refusals <- message.Error:
			default:
			}
		case message.Type == "device-metadata":
			s.metadataOnce.Do(func() { close(s.metadata) })
		case message.Type == "answer":
			select {
			case s.answers <- message.Data:
			default:
			}
		case message.Type == "new-ice-candidate":
			s.addRemoteCandidate(message.Data)
		}
	}
}

func (s *signaling) addRemoteCandidate(data json.RawMessage) {
	var candidate webrtc.ICECandidateInit
	if json.Unmarshal(data, &candidate) != nil || candidate.Candidate == "" {
		return
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	if s.pc == nil {
		s.queued = append(s.queued, candidate)
		return
	}
	_ = s.pc.AddICECandidate(candidate)
}

// negotiate waits for the KVM's device-metadata, offers to receive video
// with the "rpc" data channel that the peer already has, sends the local
// candidates and takes the KVM's answer.
func (s *signaling) negotiate(ctx context.Context, pc *webrtc.PeerConnection) error {
	if err := s.wait(ctx, s.metadata, "device-metadata"); err != nil {
		return err
	}
	if _, err := pc.AddTransceiverFromKind(webrtc.RTPCodecTypeVideo,
		webrtc.RTPTransceiverInit{Direction: webrtc.RTPTransceiverDirectionRecvonly}); err != nil {
		return err
	}
	offer, err := pc.CreateOffer(nil)
	if err != nil {
		return err
	}
	gathered := webrtc.GatheringCompletePromise(pc)
	if err := pc.SetLocalDescription(offer); err != nil {
		return err
	}
	if err := s.wait(ctx, gathered, "ICE gathering"); err != nil {
		return err
	}
	// As a browser does, the offer goes without candidates and each
	// candidate follows on its own. The KVM takes the offer before it reads
	// the next message.
	local := *pc.LocalDescription()
	candidates := localCandidates(local.SDP)
	local.SDP = withoutCandidates(local.SDP)
	description, err := json.Marshal(local)
	if err != nil {
		return err
	}
	if err := s.send(ctx, "offer", map[string]string{"sd": base64.StdEncoding.EncodeToString(description)}); err != nil {
		return err
	}
	// The video section is the first of the offer.
	mid, index := "0", uint16(0)
	for _, candidate := range candidates {
		if err := s.send(ctx, "new-ice-candidate", webrtc.ICECandidateInit{Candidate: candidate, SDPMid: &mid, SDPMLineIndex: &index}); err != nil {
			return err
		}
	}
	var encoded json.RawMessage
	select {
	case encoded = <-s.answers:
	case refusal := <-s.refusals:
		return fmt.Errorf("the KVM refused the offer: %s", refusal)
	case <-s.closed:
		return fmt.Errorf("the KVM closed the signaling before its answer: %w", s.readErr)
	case <-ctx.Done():
		return fmt.Errorf("the KVM's answer: %w", ctx.Err())
	}
	answer, err := decodeDescription(encoded)
	if err != nil {
		return fmt.Errorf("the KVM's answer: %w", err)
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	if err := pc.SetRemoteDescription(answer); err != nil {
		return err
	}
	s.pc = pc
	for _, candidate := range s.queued {
		_ = pc.AddICECandidate(candidate)
	}
	s.queued = nil
	return nil
}

// decodeDescription decodes a description as the KVM sends it: a JSON
// string with the base64 of the description's JSON.
func decodeDescription(encoded json.RawMessage) (webrtc.SessionDescription, error) {
	var description webrtc.SessionDescription
	var text string
	if err := json.Unmarshal(encoded, &text); err != nil {
		return description, err
	}
	raw, err := base64.StdEncoding.DecodeString(text)
	if err != nil {
		return description, err
	}
	err = json.Unmarshal(raw, &description)
	return description, err
}

// wait waits for ready, the step of the signaling that what names.
func (s *signaling) wait(ctx context.Context, ready <-chan struct{}, what string) error {
	select {
	case <-ready:
		return nil
	case <-s.closed:
		return fmt.Errorf("the KVM closed the signaling before %s: %w", what, s.readErr)
	case <-ctx.Done():
		return fmt.Errorf("%s: %w", what, ctx.Err())
	}
}

func (s *signaling) send(ctx context.Context, kind string, data any) error {
	return wsjson.Write(ctx, s.conn, map[string]any{"type": kind, "data": data})
}

// hangUp closes the WebSocket, also one on which negotiate did not finish.
// A Mini ends the WebRTC session with it. A JetKVM keeps the WebRTC session
// until the peer closes.
func (s *signaling) hangUp() {
	_ = s.conn.Close(websocket.StatusNormalClosure, "")
	s.stop()
}
