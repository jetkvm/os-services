package kvm

import (
	"bytes"
	"context"
	"encoding/binary"
	"encoding/json"
	"fmt"
	"os/exec"
	"regexp"
	"strconv"
	"strings"
	"sync"
	"time"

	"github.com/pion/rtcp"
	"github.com/pion/rtp/codecs"
	"github.com/pion/webrtc/v4"
	"github.com/pion/webrtc/v4/pkg/media/samplebuilder"
)

// frameWait is how long a frame with a lost packet waits for the packet to
// come again; the round trip to a KVM on the LAN is a few milliseconds.
const frameWait = 300 * time.Millisecond

// keyframeInterval is how often a recording asks the KVM for a keyframe, as
// a browser asks: until the first keyframe arrives, and after a frame was
// lost for good until the next keyframe arrives. Asking while the stream is
// whole would add keyframes that a viewer does not get.
const keyframeInterval = time.Second

// videoTap keeps the H.264 track and, while a recording runs, its stream as
// Annex B with the arrival time of each access unit. A recording starts at
// the first access unit with a sequence parameter set, so that the decoder
// can decode every frame it gets.
type videoTap struct {
	mu        sync.Mutex
	pc        *webrtc.PeerConnection
	ssrc      webrtc.SSRC
	recording *Recording
}

// Recording is a recorded H.264 stream: its access units in Annex B, when
// the last RTP packet of each arrived here and the RTP timestamp of its
// packets, how many RTP packets arrived, and how many never arrived, also not
// as a retransmission.
type Recording struct {
	Units      [][]byte
	Arrivals   []time.Time
	Timestamps []uint32
	Packets    int
	Lost       int
	// broken is set when a frame was lost for good and no keyframe has
	// arrived since.
	broken bool
	// The sequence numbers that arrived, extended past 16 bits; seq is the
	// last one as it arrived and ext its extension.
	seen        map[int64]bool
	seq         uint16
	ext         int64
	first, last int64
}

// packet counts an RTP packet that arrived. A packet is lost when no packet
// with its sequence number arrived, between the first and the last that did.
func (r *Recording) packet(seq uint16) {
	if r.seen == nil {
		r.seen = map[int64]bool{}
		r.ext, r.first, r.last = int64(seq), int64(seq), int64(seq)
	} else {
		r.ext += int64(int16(seq - r.seq))
	}
	r.seq = seq
	r.seen[r.ext] = true
	r.first, r.last = min(r.first, r.ext), max(r.last, r.ext)
	r.Packets = len(r.seen)
	r.Lost = int(r.last-r.first+1) - r.Packets
}

// Stream is the recording as one Annex B stream.
func (r Recording) Stream() []byte { return bytes.Join(r.Units, nil) }

// hasSPS reports whether an Annex B access unit has a sequence parameter set
// (NAL unit type 7).
func hasSPS(unit []byte) bool {
	for i := 0; i+3 < len(unit); i++ {
		if unit[i] == 0 && unit[i+1] == 0 && unit[i+2] == 1 && unit[i+3]&0x1f == 7 {
			return true
		}
	}
	return false
}

func (t *videoTap) read(pc *webrtc.PeerConnection, track *webrtc.TrackRemote) {
	t.mu.Lock()
	t.pc, t.ssrc = pc, track.SSRC()
	t.mu.Unlock()
	// The builder drops a frame that needs more packets than it holds, and a
	// keyframe of a detailed 1920x1080 desktop has about 600; 8192 holds a 4K
	// keyframe. A frame with a lost packet waits for its retransmission at
	// most frameWait, as a browser's jitter buffer waits, and is dropped
	// after; a limit in packets would wait seconds when few frames come.
	builder := samplebuilder.New(8192, &codecs.H264Packet{}, track.Codec().ClockRate,
		samplebuilder.WithMaxTimeDelay(frameWait))
	// The builder hands out an access unit only when the next one starts, so
	// its arrival is when the last of its packets arrived, which need not be
	// the one with the marker when packets come out of order.
	arrived := map[uint32]time.Time{}
	for {
		packet, _, err := track.ReadRTP()
		if err != nil {
			return
		}
		arrived[packet.Timestamp] = time.Now()
		t.mu.Lock()
		if r := t.recording; r != nil {
			r.packet(packet.SequenceNumber)
		}
		t.mu.Unlock()
		builder.Push(packet)
		for sample := builder.Pop(); sample != nil; sample = builder.Pop() {
			at, ok := arrived[sample.PacketTimestamp]
			if !ok {
				at = time.Now()
			}
			delete(arrived, sample.PacketTimestamp)
			t.mu.Lock()
			if r := t.recording; r != nil {
				r.broken = (r.broken || sample.PrevDroppedPackets > 0) && !hasSPS(sample.Data)
			}
			if r := t.recording; r != nil && (len(r.Arrivals) > 0 || hasSPS(sample.Data)) {
				r.Units = append(r.Units, sample.Data)
				r.Arrivals = append(r.Arrivals, at)
				r.Timestamps = append(r.Timestamps, sample.PacketTimestamp)
			}
			t.mu.Unlock()
		}
	}
}

func (t *videoTap) askKeyframe() {
	t.mu.Lock()
	pc, ssrc := t.pc, t.ssrc
	t.mu.Unlock()
	if pc != nil {
		_ = pc.WriteRTCP([]rtcp.Packet{&rtcp.PictureLossIndication{MediaSSRC: uint32(ssrc)}})
	}
}

// record returns the stream that arrives during d.
func (t *videoTap) record(ctx context.Context, d time.Duration) (Recording, error) {
	t.mu.Lock()
	t.recording = &Recording{}
	t.mu.Unlock()
	end := time.After(d)
	ticker := time.NewTicker(keyframeInterval)
	defer ticker.Stop()
	t.askKeyframe()
	var err error
wait:
	for {
		select {
		case <-ticker.C:
			t.mu.Lock()
			ask := len(t.recording.Arrivals) == 0 || t.recording.broken
			t.mu.Unlock()
			if ask {
				t.askKeyframe()
			}
		case <-end:
			break wait
		case <-ctx.Done():
			err = ctx.Err()
			break wait
		}
	}
	t.mu.Lock()
	recorded := *t.recording
	t.recording = nil
	t.mu.Unlock()
	if err == nil && len(recorded.Arrivals) == 0 {
		err = fmt.Errorf("no H.264 keyframe arrived in %s", d)
	}
	return recorded, err
}

// Record records the video that the KVM sends for d.
func (k *KVM) Record(ctx context.Context, d time.Duration) (Recording, error) {
	return k.video.record(ctx, d)
}

// Video records the video that the KVM sends for d and decodes it.
func (k *KVM) Video(ctx context.Context, d time.Duration) (VideoReport, error) {
	recorded, err := k.video.record(ctx, d)
	if err != nil {
		return VideoReport{}, err
	}
	duration := time.Since(recorded.Arrivals[0])
	report, err := Decode(ctx, recorded.Stream())
	report.Packets, report.Lost, report.Duration = recorded.Packets, recorded.Lost, duration
	return report, err
}

// Decode counts the frames of an H.264 stream with ffprobe, which decodes
// every frame and writes the decoder's errors to stderr.
func Decode(ctx context.Context, stream []byte) (VideoReport, error) {
	var probe, errors bytes.Buffer
	command := exec.CommandContext(ctx, "ffprobe", "-v", "error", "-f", "h264", "-count_frames",
		"-select_streams", "v:0", "-show_entries", "stream=width,height,nb_read_frames", "-of", "json", "-")
	command.Stdin, command.Stdout, command.Stderr = bytes.NewReader(stream), &probe, &errors
	if err := command.Run(); err != nil {
		return VideoReport{}, fmt.Errorf("ffprobe: %w: %s", err, strings.TrimSpace(errors.String()))
	}
	var probed struct {
		Streams []struct {
			Width  int    `json:"width"`
			Height int    `json:"height"`
			Frames string `json:"nb_read_frames"`
		} `json:"streams"`
	}
	if err := json.Unmarshal(probe.Bytes(), &probed); err != nil || len(probed.Streams) != 1 {
		return VideoReport{}, fmt.Errorf("ffprobe: no video stream in %d bytes", len(stream))
	}
	report := VideoReport{Width: probed.Streams[0].Width, Height: probed.Streams[0].Height, Bytes: len(stream)}
	fmt.Sscan(probed.Streams[0].Frames, &report.Frames)
	for _, line := range strings.Split(strings.TrimSpace(errors.String()), "\n") {
		if line != "" {
			report.Errors = append(report.Errors, line)
		}
	}
	return report, nil
}

// Crop is one decoded frame of Crops: the access unit it came from, and
// the luma of the squares side by side.
type Crop struct {
	Unit int
	Luma []byte
}

// showinfoPTS matches the timestamp in a frame line of FFmpeg's showinfo
// filter.
var showinfoPTS = regexp.MustCompile(`Parsed_showinfo.* n: *\d+ pts: *(\d+)`)

// Crops decodes the access units of r and returns each decoded frame with
// the luma of size x size squares at the top-left corners in corners (two
// or more), side by side: one image of width size*len(corners) and height
// size per frame. Not every access unit decodes to a frame, so each unit
// goes to FFmpeg as one packet of an IVF file with its index as timestamp,
// and the showinfo filter reports the timestamp of each frame.
func Crops(ctx context.Context, r Recording, size int, corners [][2]int) ([]Crop, error) {
	var ivf bytes.Buffer
	ivf.WriteString("DKIF")
	// Version, header size, codec, width and height (from the stream), a
	// time base of 1/1000, the frame count and a reserved word.
	_ = binary.Write(&ivf, binary.LittleEndian, struct {
		Version, HeaderSize uint16
		Codec               [4]byte
		Width, Height       uint16
		Rate, Scale, Frames uint32
		Reserved            uint32
	}{0, 32, [4]byte{'H', '2', '6', '4'}, 0, 0, 1000, 1, uint32(len(r.Units)), 0})
	for i, unit := range r.Units {
		_ = binary.Write(&ivf, binary.LittleEndian, struct {
			Size uint32
			PTS  uint64
		}{uint32(len(unit)), uint64(i)})
		ivf.Write(unit)
	}
	var graph strings.Builder
	fmt.Fprintf(&graph, "[0:v]showinfo,split=%d", len(corners))
	for i := range corners {
		fmt.Fprintf(&graph, "[s%d]", i)
	}
	for i, c := range corners {
		fmt.Fprintf(&graph, ";[s%d]crop=%d:%d:%d:%d[c%d]", i, size, size, c[0], c[1], i)
	}
	graph.WriteString(";")
	for i := range corners {
		fmt.Fprintf(&graph, "[c%d]", i)
	}
	fmt.Fprintf(&graph, "hstack=inputs=%d,format=gray", len(corners))
	var frames, log bytes.Buffer
	command := exec.CommandContext(ctx, "ffmpeg", "-hide_banner", "-nostats", "-loglevel", "info", "-f", "ivf", "-i", "-",
		"-filter_complex", graph.String(), "-fps_mode", "passthrough", "-f", "rawvideo", "-")
	command.Stdin, command.Stdout, command.Stderr = &ivf, &frames, &log
	if err := command.Run(); err != nil {
		return nil, fmt.Errorf("ffmpeg: %w: %s", err, strings.TrimSpace(log.String()))
	}
	var units []int
	for _, match := range showinfoPTS.FindAllStringSubmatch(log.String(), -1) {
		unit, _ := strconv.Atoi(match[1])
		units = append(units, unit)
	}
	frameSize := size * size * len(corners)
	if frames.Len() != len(units)*frameSize {
		return nil, fmt.Errorf("ffmpeg: %d bytes of frames for %d frame timestamps", frames.Len(), len(units))
	}
	crops := make([]Crop, len(units))
	for i, unit := range units {
		if unit >= len(r.Units) {
			return nil, fmt.Errorf("ffmpeg: a frame of access unit %d of %d", unit, len(r.Units))
		}
		crops[i] = Crop{Unit: unit, Luma: frames.Bytes()[i*frameSize : (i+1)*frameSize]}
	}
	return crops, nil
}
