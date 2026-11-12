package kvm

import "testing"

func TestLostPacketsAreSequenceNumbersThatNeverArrived(t *testing.T) {
	for _, c := range []struct {
		name          string
		arrived       []uint16
		packets, lost int
	}{
		{"whole", []uint16{10, 11, 12, 13}, 4, 0},
		{"one missing", []uint16{10, 11, 13, 14}, 4, 1},
		{"retransmitted late", []uint16{10, 11, 13, 14, 12}, 5, 0},
		{"duplicate", []uint16{10, 11, 11, 12}, 3, 0},
		{"out of order", []uint16{11, 10, 12}, 3, 0},
		{"across the wrap", []uint16{65534, 65535, 1, 2}, 4, 1},
		{"retransmitted across the wrap", []uint16{65535, 1, 0}, 3, 0},
	} {
		var r Recording
		for _, seq := range c.arrived {
			r.packet(seq)
		}
		if r.Packets != c.packets || r.Lost != c.lost {
			t.Errorf("%s: %d arrived, %d lost; want %d, %d", c.name, r.Packets, r.Lost, c.packets, c.lost)
		}
	}
}
