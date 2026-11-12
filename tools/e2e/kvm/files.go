package kvm

import (
	"context"
	"encoding/binary"
	"encoding/json"
	"fmt"
	"strings"
	"time"

	"github.com/pion/webrtc/v4"
)

const (
	// fileChunk is the largest piece of a file in one message, as the KVM's
	// web page sends it.
	fileChunk = 32 * 1024
	// fileBuffered is how much the data channel may hold unsent.
	fileBuffered = 96 * 1024
)

// fileMessage is a text message of a file drop channel: the KVM gives
// credit for bytes and reports each file's end.
type fileMessage struct {
	Type     string `json:"type"` // credit or done
	Transfer uint32 `json:"transfer"`
	Bytes    int    `json:"bytes"`
	Status   string `json:"status"`
	Path     string `json:"path"`
}

// DropFile sends one file to the computer and returns the path where the
// service wrote it. It asks for a drop of the file, then sends it on the
// channel that the KVM names: each binary message is the transfer ID (u32,
// little endian) and up to fileChunk bytes, within the KVM's credit.
func (k *KVM) DropFile(ctx context.Context, name string, data []byte) (string, error) {
	var drop struct {
		Channel   string   `json:"channel"`
		Transfers []uint32 `json:"transfers"`
	}
	params := map[string]any{"drop": map[string]any{"files": []map[string]any{{"name": name, "size": len(data)}}}}
	if err := k.rpc(ctx, "guestFileDrop", params, &drop); err != nil {
		return "", err
	}
	if !strings.HasPrefix(drop.Channel, "guestfile_") || len(drop.Transfers) != 1 {
		return "", fmt.Errorf("guestFileDrop: unexpected answer: channel %q, transfers %v", drop.Channel, drop.Transfers)
	}
	channel, err := k.pc.CreateDataChannel(drop.Channel, nil)
	if err != nil {
		return "", err
	}
	defer channel.Close()
	raw, err := channel.Detach()
	if err != nil {
		return "", err
	}
	// finished lets a message that arrives after the transfer go.
	messages, finished := make(chan fileMessage), make(chan struct{})
	defer close(finished)
	go readMessages(raw, func(data []byte, isString bool) {
		var parsed fileMessage
		if isString && json.Unmarshal(data, &parsed) == nil {
			select {
			case messages <- parsed:
			case <-finished:
			}
		}
	})
	return sendFile(ctx, channel, drop.Transfers[0], data, messages)
}

func sendFile(ctx context.Context, channel *webrtc.DataChannel, transfer uint32, data []byte, messages <-chan fileMessage) (string, error) {
	credit, sent := 0, 0
	for {
		// While the channel's buffer is full, wait for it in the select
		// below, so that messages and ctx are still read.
		var drained <-chan time.Time
		for sent < len(data) && credit > 0 && channel.ReadyState() == webrtc.DataChannelStateOpen {
			if channel.BufferedAmount() > fileBuffered {
				drained = time.After(20 * time.Millisecond)
				break
			}
			n := min(fileChunk, credit, len(data)-sent)
			message := make([]byte, 4+n)
			binary.LittleEndian.PutUint32(message, transfer)
			copy(message[4:], data[sent:sent+n])
			if err := channel.Send(message); err != nil {
				return "", err
			}
			sent += n
			credit -= n
		}
		select {
		case message := <-messages:
			switch {
			case message.Transfer != transfer:
			case message.Type == "credit":
				credit += message.Bytes
			case message.Type == "done" && message.Status == "ok":
				return message.Path, nil
			case message.Type == "done":
				return "", fmt.Errorf("file transfer ended with status %q after %d of %d bytes", message.Status, sent, len(data))
			}
		case <-drained:
		case <-ctx.Done():
			return "", fmt.Errorf("file transfer: %d of %d bytes sent: %w", sent, len(data), ctx.Err())
		}
	}
}
