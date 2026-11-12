// Package probe is the layout of the two windows that perf opens on the
// computer under test, which the windows and perf share. Positions and sizes
// are fractions of the screen's width and height.
//
// The noise window changes a grid of gray blocks every few milliseconds, so
// that the screen changes at each refresh and the service captures and
// encodes at full rate, with the same load on every system.
//
// The key window shows five blocks in a row. Each block is black or white,
// and together they encode the last letter typed: a is 1 and z is 26, the
// first block is the highest bit, and white is 1. Before the first letter
// all blocks are black.
package probe

import (
	"fmt"
	"time"
)

// EDIDVendor is the manufacturer id "JET" in the EDID that the KVM presents,
// as EDID bytes 8 and 9 read big-endian. The service captures the screen
// with this EDID, so the windows open on it.
const EDIDVendor = 0x28b4

// The noise window: the left half of the screen, below the Mac's menu bar.
const (
	NoiseLeft   = 0.05
	NoiseTop    = 0.1
	NoiseWidth  = 0.5
	NoiseHeight = 0.5
	// NoiseColumns x NoiseRows blocks, each of one gray at a time.
	NoiseColumns = 24
	NoiseRows    = 14
	// NoiseInterval is the time between two frames of noise, shorter than
	// one refresh.
	NoiseInterval = 4 * time.Millisecond
)

// NoiseArgs are the noise window's position, size and grid for a helper's
// command line.
func NoiseArgs() string {
	return fmt.Sprintf("%g %g %g %g %d %d %d", NoiseLeft, NoiseTop, NoiseWidth, NoiseHeight,
		NoiseColumns, NoiseRows, NoiseInterval.Milliseconds())
}

// The key window: on the right, below the Mac's notifications and clear of
// the noise window.
const (
	KeyLeft   = 0.72
	KeyTop    = 0.35
	KeyWidth  = 0.24
	KeyHeight = 0.14
)

// SecretText is what the secret helper puts on the computer's clipboard,
// marked as secret as password managers mark a password: on Linux with the
// type x-kde-passwordManagerHint, on macOS with org.nspasteboard.ConcealedType,
// and on Windows with the format ExcludeClipboardContentFromMonitorProcessing.
// The macOS and Windows helpers get it on their command line, so it has no
// spaces.
const SecretText = "os-services-e2e-secret"

// Blocks is the number of blocks of the key window, enough for 26 letters.
const Blocks = 5

// BlockCenter is the center of block i as fractions of the key window's
// width and height: the blocks sit in five equal columns, halfway down.
func BlockCenter(i int) (x, y float64) { return 0.1 + 0.2*float64(i), 0.5 }

// BlockSide is the side of each square block as a fraction of the key
// window's width.
const BlockSide = 0.12

// Place is where a window is on the KVM's screen, as fractions of the
// screen's width and height.
type Place struct{ Left, Top, Width, Height float64 }

// KeyPlace is where the key window asks to be. A window manager may put it
// elsewhere; the window then reports where it is.
func KeyPlace() Place { return Place{KeyLeft, KeyTop, KeyWidth, KeyHeight} }

// KeyWindow is where the key window is twice over: on the KVM's screen,
// where perf reads its blocks, and on the whole desktop, which the KVM's
// absolute pointer covers on X11. The two differ on a desktop with more
// than one screen.
type KeyWindow struct{ Screen, Desktop Place }

// DefaultKeyWindow is the key window at its own place, for a window that
// reports none. On macOS and Windows, the KVM's pointer covers the main
// screen, which is the KVM's.
func DefaultKeyWindow() KeyWindow { return KeyWindow{KeyPlace(), KeyPlace()} }

// Focus is where a click gives the key window at p the keyboard focus, as
// fractions of the screen: in the window's lower left corner, away from the
// blocks.
func Focus(p Place) (x, y float64) { return p.Left + 0.03*p.Width, p.Top + 0.9*p.Height }

// KeyArgs are the key window's position and size for a helper's command
// line.
func KeyArgs() string { return fmt.Sprintf("%g %g %g %g", KeyLeft, KeyTop, KeyWidth, KeyHeight) }

// Value is the number of a letter, 1 to 26, or 0 for anything else.
func Value(letter rune) int {
	if letter < 'a' || letter > 'z' {
		return 0
	}
	return int(letter-'a') + 1
}

// White reports whether block i is white for value.
func White(value, i int) bool { return value>>(Blocks-1-i)&1 == 1 }
