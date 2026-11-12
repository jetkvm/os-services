package kvm

import (
	"crypto/ecdsa"
	"crypto/elliptic"
	"crypto/rand"
	"crypto/x509"
	"crypto/x509/pkix"
	"fmt"
	"math/big"
	"net"
	"net/url"
	"strings"
	"time"

	"github.com/pion/ice/v4"
	"github.com/pion/interceptor"
	"github.com/pion/webrtc/v4"
)

// newPeer takes only H.264 video, so the KVM sends video that the decoder
// reads: a JetKVM sends H.265 to a peer that offers it.
//
// It gathers one candidate: UDP on the IPv4 address that routes to the KVM,
// as the KVM's web page sends. The Mini learns a peer-reflexive pair from
// each source of connectivity checks and starts DTLS again when the pair
// changes, so checks from other interfaces break the handshake.
func newPeer(base string) (*webrtc.PeerConnection, error) {
	local, err := localAddress(base)
	if err != nil {
		return nil, err
	}
	settings := webrtc.SettingEngine{}
	settings.SetNetworkTypes([]webrtc.NetworkType{webrtc.NetworkTypeUDP4})
	settings.SetIPFilter(func(ip net.IP) bool { return ip.Equal(local) })
	settings.SetICEMulticastDNSMode(ice.MulticastDNSModeDisabled)
	settings.SetIncludeLoopbackCandidate(local.IsLoopback())
	// pion opens a data channel that this side creates before
	// CreateDataChannel returns and drops the messages that arrive before
	// OnMessage is set, such as the Mini's first file credit. A detached
	// channel keeps them until readMessages reads them.
	settings.DetachDataChannels()
	engine := &webrtc.MediaEngine{}
	if err := engine.RegisterCodec(webrtc.RTPCodecParameters{
		RTPCodecCapability: webrtc.RTPCodecCapability{
			MimeType: webrtc.MimeTypeH264, ClockRate: 90000,
			SDPFmtpLine:  "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f",
			RTCPFeedback: []webrtc.RTCPFeedback{{Type: "nack"}, {Type: "nack", Parameter: "pli"}},
		},
		PayloadType: 102,
	}, webrtc.RTPCodecTypeVideo); err != nil {
		return nil, err
	}
	certificate, err := dtlsCertificate()
	if err != nil {
		return nil, err
	}
	// As a browser does, the client asks the KVM again for lost packets
	// (NACK) and sends receiver reports. Without it a lost packet is never
	// sent again: a keyframe with one lost packet is lost whole, and the
	// sample builder holds the frames behind the gap until 512 more packets
	// arrive, seconds on a still screen.
	interceptors := &interceptor.Registry{}
	if err := webrtc.RegisterDefaultInterceptors(engine, interceptors); err != nil {
		return nil, err
	}
	return webrtc.NewAPI(webrtc.WithMediaEngine(engine), webrtc.WithSettingEngine(settings), webrtc.WithInterceptorRegistry(interceptors)).
		NewPeerConnection(webrtc.Configuration{Certificates: []webrtc.Certificate{*certificate}})
}

// dtlsCertificate is a self-signed DTLS certificate named CN=dtls_srtp. The
// Mini's certificate request names its own certificate, CN=dtls_srtp, as the
// one acceptable CA, and pion sends no certificate whose issuer is not on
// that list. The Mini then ends the handshake. (A browser sends its
// certificate whatever the list says.) The Mini checks the certificate
// against the fingerprint in the answer, as for a browser.
func dtlsCertificate() (*webrtc.Certificate, error) {
	key, err := ecdsa.GenerateKey(elliptic.P256(), rand.Reader)
	if err != nil {
		return nil, err
	}
	name := pkix.Name{CommonName: "dtls_srtp"}
	return webrtc.NewCertificate(key, x509.Certificate{
		SerialNumber: big.NewInt(time.Now().UnixNano()),
		Subject:      name,
		Issuer:       name,
		NotBefore:    time.Now().Add(-time.Hour),
		NotAfter:     time.Now().Add(24 * time.Hour),
	})
}

// localAddress is this computer's IPv4 address on the route to the KVM.
func localAddress(base string) (net.IP, error) {
	parsed, err := url.Parse(base)
	if err != nil {
		return nil, err
	}
	port := parsed.Port()
	if port == "" {
		port = "80"
	}
	// A UDP "connection" sends nothing; it only picks the route.
	conn, err := net.Dial("udp4", net.JoinHostPort(parsed.Hostname(), port))
	if err != nil {
		return nil, fmt.Errorf("no IPv4 route to the KVM: %w", err)
	}
	defer conn.Close()
	return conn.LocalAddr().(*net.UDPAddr).IP, nil
}

// readMessages hands each message of a detached data channel to handle,
// until the channel closes.
func readMessages(raw interface {
	ReadDataChannel([]byte) (int, bool, error)
}, handle func(data []byte, isString bool)) {
	buffer := make([]byte, 1<<20)
	for {
		n, isString, err := raw.ReadDataChannel(buffer)
		if err != nil {
			return
		}
		handle(append([]byte(nil), buffer[:n]...), isString)
	}
}

// localCandidates are the candidate lines of an SDP for component 1, without
// "a=". pion lists each candidate again for component 2, the RTCP component
// that rtcp-mux leaves unused, and the Mini refuses those.
func localCandidates(sdp string) []string {
	var candidates []string
	for _, line := range strings.Split(strings.ReplaceAll(sdp, "\r\n", "\n"), "\n") {
		if !strings.HasPrefix(line, "a=candidate:") {
			continue
		}
		if fields := strings.Fields(line); len(fields) > 1 && fields[1] == "1" {
			candidates = append(candidates, strings.TrimPrefix(line, "a="))
		}
	}
	return candidates
}

// withoutCandidates removes the candidate lines from an SDP: the KVM takes
// candidates only as separate signals, which the Mini filters.
func withoutCandidates(sdp string) string {
	var kept []string
	for _, line := range strings.Split(strings.ReplaceAll(sdp, "\r\n", "\n"), "\n") {
		if strings.HasPrefix(line, "a=candidate:") || line == "a=end-of-candidates" {
			continue
		}
		kept = append(kept, line)
	}
	return strings.TrimRight(strings.Join(kept, "\r\n"), "\r\n") + "\r\n"
}
