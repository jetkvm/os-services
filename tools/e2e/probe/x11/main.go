// Command x11 is perf's windows for X11 and Xwayland (see package probe):
//
//	x11 noise window.log    the noise window
//	x11 keys window.log     the key window
//	x11 secret window.log   probe.SecretText on the clipboard, marked as secret
//
// Both windows open on the screen whose RandR output has the KVM's EDID, or
// on the only screen when no output has an EDID, as under Xwayland. Each
// appends "ready" to window.log once it shows. The noise window is
// override-redirect, so that a window manager does not move or decorate it.
// The key window is a normal window, because a Wayland compositor gives the
// keyboard focus only to a window that it manages. It asks for its place,
// writes the place it got after "ready", on the KVM's screen and on the
// whole desktop, again after each move, takes the
// keyboard focus, takes it
// back when it loses it, and appends a line to window.log each time it lost
// the focus.
package main

import (
	"fmt"
	"math/rand/v2"
	"os"
	"time"

	"github.com/jezek/xgb"
	"github.com/jezek/xgb/randr"
	"github.com/jezek/xgb/xproto"

	"github.com/jetkvm/os-services/tools/e2e/probe"
)

func main() {
	var err error
	switch {
	case len(os.Args) == 3 && os.Args[1] == "noise":
		err = noise(os.Args[2])
	case len(os.Args) == 3 && os.Args[1] == "keys":
		err = keys(os.Args[2])
	case len(os.Args) == 3 && os.Args[1] == "secret":
		err = secret(os.Args[2])
	default:
		fmt.Fprintln(os.Stderr, "usage: x11 noise window.log | x11 keys window.log | x11 secret window.log")
		os.Exit(2)
	}
	if err != nil {
		fmt.Fprintln(os.Stderr, "x11:", err)
		os.Exit(1)
	}
}

// window is a window at a place on the KVM's screen.
type window struct {
	conn   *xgb.Conn
	id     xproto.Window
	root   xproto.Window
	kvm    *randr.GetCrtcInfoReply
	gc     xproto.Gcontext
	width  uint16
	height uint16
}

// open opens a window at a place on the KVM's screen: override-redirect, or
// managed by the window manager, which places it where it asks to be when
// the window manager follows the request.
func open(left, top, width, height float64, events uint32, managed bool) (*window, error) {
	conn, err := xgb.NewConn()
	if err != nil {
		return nil, err
	}
	screen := xproto.Setup(conn).DefaultScreen(conn)
	kvm, err := kvmScreen(conn, screen.Root)
	if err != nil {
		return nil, err
	}
	sx, sy, sw, sh := float64(kvm.X), float64(kvm.Y), float64(kvm.Width), float64(kvm.Height)
	w := &window{conn: conn, root: screen.Root, kvm: kvm, width: uint16(width * sw), height: uint16(height * sh)}
	if w.id, err = xproto.NewWindowId(conn); err != nil {
		return nil, err
	}
	x, y := int16(sx+left*sw), int16(sy+top*sh)
	mask, values := uint32(xproto.CwBackPixel|xproto.CwOverrideRedirect|xproto.CwEventMask), []uint32{0x808080, 1, events}
	if managed {
		mask, values = xproto.CwBackPixel|xproto.CwEventMask, []uint32{0x808080, events}
	}
	if err := xproto.CreateWindowChecked(conn, screen.RootDepth, w.id, screen.Root,
		x, y, w.width, w.height, 0, xproto.WindowClassInputOutput, screen.RootVisual, mask, values).Check(); err != nil {
		return nil, err
	}
	if managed {
		// WM_NORMAL_HINTS with USPosition and USSize: the place comes from
		// the user, which window managers follow.
		hints := make([]byte, 18*4)
		for i, v := range []uint32{1 | 2, uint32(x), uint32(y), uint32(w.width), uint32(w.height)} {
			xgb.Put32(hints[i*4:], v)
		}
		if err := xproto.ChangePropertyChecked(conn, xproto.PropModeReplace, w.id, xproto.AtomWmNormalHints,
			xproto.AtomWmSizeHints, 32, 18, hints).Check(); err != nil {
			return nil, err
		}
	}
	if w.gc, err = xproto.NewGcontextId(conn); err != nil {
		return nil, err
	}
	if err := xproto.CreateGCChecked(conn, w.gc, xproto.Drawable(w.id), 0, nil).Check(); err != nil {
		return nil, err
	}
	return w, xproto.MapWindowChecked(conn, w.id).Check()
}

// screenWait is how long kvmScreen looks for the KVM's screen. It is less
// than the time that perf waits for "ready".
const screenWait = 20 * time.Second

// kvmScreen is the place on the root window of the one connected RandR
// output whose EDID has the KVM's manufacturer id. Xwayland gives its
// outputs no EDID; there, the only connected output is the KVM's. While the
// KVM changes its EDID, the X server has no connected output with that
// EDID, so kvmScreen looks again until screenWait is over.
func kvmScreen(conn *xgb.Conn, root xproto.Window) (*randr.GetCrtcInfoReply, error) {
	if err := randr.Init(conn); err != nil {
		return nil, fmt.Errorf("RandR: %w", err)
	}
	for end := time.Now().Add(screenWait); ; time.Sleep(250 * time.Millisecond) {
		screen, err := kvmScreenNow(conn, root)
		if err == nil || time.Now().After(end) {
			return screen, err
		}
	}
}

// kvmScreenNow is kvmScreen for the outputs as the X server has them now.
func kvmScreenNow(conn *xgb.Conn, root xproto.Window) (*randr.GetCrtcInfoReply, error) {
	resources, err := randr.GetScreenResourcesCurrent(conn, root).Reply()
	if err != nil {
		return nil, err
	}
	name := "EDID"
	atom, err := xproto.InternAtom(conn, true, uint16(len(name)), name).Reply()
	if err != nil {
		return nil, err
	}
	var screens, connected []*randr.GetCrtcInfoReply
	edids := 0
	for _, output := range resources.Outputs {
		info, err := randr.GetOutputInfo(conn, output, resources.ConfigTimestamp).Reply()
		if err != nil || info.Connection != randr.ConnectionConnected || info.Crtc == 0 {
			continue
		}
		crtc, err := randr.GetCrtcInfo(conn, info.Crtc, resources.ConfigTimestamp).Reply()
		if err != nil {
			return nil, err
		}
		connected = append(connected, crtc)
		edid, err := randr.GetOutputProperty(conn, output, atom.Atom, xproto.AtomAny, 0, 32, false, false).Reply()
		if err != nil || len(edid.Data) == 0 {
			continue
		}
		edids++
		if len(edid.Data) >= 10 && uint16(edid.Data[8])<<8|uint16(edid.Data[9]) == probe.EDIDVendor {
			screens = append(screens, crtc)
		}
	}
	if edids == 0 && len(connected) == 1 {
		return connected[0], nil
	}
	if len(screens) != 1 {
		return nil, fmt.Errorf("%d screens show the KVM's EDID, not one", len(screens))
	}
	return screens[0], nil
}

// place is where the window's content is, after the window manager placed
// it: on the KVM's screen and on the whole desktop, the root window.
func (w *window) place() (probe.KeyWindow, error) {
	at, err := xproto.TranslateCoordinates(w.conn, w.id, w.root, 0, 0).Reply()
	if err != nil {
		return probe.KeyWindow{}, err
	}
	size, err := xproto.GetGeometry(w.conn, xproto.Drawable(w.id)).Reply()
	if err != nil {
		return probe.KeyWindow{}, err
	}
	root, err := xproto.GetGeometry(w.conn, xproto.Drawable(w.root)).Reply()
	if err != nil {
		return probe.KeyWindow{}, err
	}
	within := func(x, y, width, height float64) probe.Place {
		return probe.Place{
			Left: (float64(at.DstX) - x) / width, Top: (float64(at.DstY) - y) / height,
			Width: float64(size.Width) / width, Height: float64(size.Height) / height,
		}
	}
	return probe.KeyWindow{
		Screen:  within(float64(w.kvm.X), float64(w.kvm.Y), float64(w.kvm.Width), float64(w.kvm.Height)),
		Desktop: within(0, 0, float64(root.Width), float64(root.Height)),
	}, nil
}

// fill fills rectangles in one color.
func (w *window) fill(color uint32, rects []xproto.Rectangle) {
	xproto.ChangeGC(w.conn, w.gc, xproto.GcForeground, []uint32{color})
	xproto.PolyFillRectangle(w.conn, xproto.Drawable(w.id), w.gc, rects)
}

// noise redraws the grid every probe.NoiseInterval with one of 60 frames
// made at the start. Each block takes one of 16 grays, and the blocks of one
// gray go in one request.
func noise(log string) error {
	w, err := open(probe.NoiseLeft, probe.NoiseTop, probe.NoiseWidth, probe.NoiseHeight, xproto.EventMaskStructureNotify, false)
	if err != nil {
		return err
	}
	defer w.conn.Close()
	note(log, "ready")
	const levels = 16
	cell := func(column, row int) xproto.Rectangle {
		x0, x1 := column*int(w.width)/probe.NoiseColumns, (column+1)*int(w.width)/probe.NoiseColumns
		y0, y1 := row*int(w.height)/probe.NoiseRows, (row+1)*int(w.height)/probe.NoiseRows
		return xproto.Rectangle{X: int16(x0), Y: int16(y0), Width: uint16(x1 - x0), Height: uint16(y1 - y0)}
	}
	frames := make([][levels][]xproto.Rectangle, 60)
	for i := range frames {
		for row := range probe.NoiseRows {
			for column := range probe.NoiseColumns {
				level := rand.N(levels)
				frames[i][level] = append(frames[i][level], cell(column, row))
			}
		}
	}
	ticker := time.NewTicker(probe.NoiseInterval)
	defer ticker.Stop()
	for i := 0; ; i++ {
		for level, rects := range frames[i%len(frames)] {
			gray := uint32(level * 255 / (levels - 1))
			w.fill(gray<<16|gray<<8|gray, rects)
		}
		if _, err := xproto.GetInputFocus(w.conn).Reply(); err != nil {
			return fmt.Errorf("the X server closed the connection: %w", err)
		}
		<-ticker.C
	}
}

// keys shows the last letter typed in the key window's blocks.
func keys(log string) error {
	w, err := open(probe.KeyLeft, probe.KeyTop, probe.KeyWidth, probe.KeyHeight,
		xproto.EventMaskExposure|xproto.EventMaskKeyPress|xproto.EventMaskFocusChange|xproto.EventMaskStructureNotify, true)
	if err != nil {
		return err
	}
	defer w.conn.Close()
	keysyms, perCode, minCode, err := keyboardMap(w.conn)
	if err != nil {
		return err
	}
	value := 0
	draw := func() {
		side := uint16(probe.BlockSide * float64(w.width))
		for i := range probe.Blocks {
			cx, cy := probe.BlockCenter(i)
			color := uint32(0x000000)
			if probe.White(value, i) {
				color = 0xffffff
			}
			w.fill(color, []xproto.Rectangle{{
				X: int16(cx*float64(w.width)) - int16(side/2), Y: int16(cy*float64(w.height)) - int16(side/2), Width: side, Height: side,
			}})
		}
		w.conn.Sync()
	}
	var mapped, shown bool
	var last probe.KeyWindow
	report := func() error {
		place, err := w.place()
		if err == nil && place != last {
			last = place
			s, d := place.Screen, place.Desktop
			note(log, fmt.Sprintf("ready %g %g %g %g %g %g %g %g", s.Left, s.Top, s.Width, s.Height, d.Left, d.Top, d.Width, d.Height))
		}
		return err
	}
	focus := func() {
		xproto.SetInputFocus(w.conn, xproto.InputFocusParent, w.id, xproto.TimeCurrentTime)
		xproto.ConfigureWindow(w.conn, w.id, xproto.ConfigWindowStackMode, []uint32{xproto.StackModeAbove})
	}
	for {
		event, err := w.conn.WaitForEvent()
		if err != nil {
			continue
		}
		if event == nil {
			return fmt.Errorf("the X server closed the connection")
		}
		switch e := event.(type) {
		case xproto.MapNotifyEvent:
			mapped = true
			focus()
		case xproto.ConfigureNotifyEvent:
			w.width, w.height = e.Width, e.Height
			if shown {
				if err := report(); err != nil {
					return err
				}
			}
		case xproto.ExposeEvent:
			draw()
			// A window manager may move the content after the map, for
			// example below a title bar, so the place counts once the window
			// shows, and again after each move.
			if mapped && !shown {
				shown = true
				if err := report(); err != nil {
					return err
				}
			}
		case xproto.KeyPressEvent:
			index := int(e.Detail-minCode) * int(perCode)
			if index >= 0 && index < len(keysyms) {
				if v := probe.Value(rune(keysyms[index])); v != 0 {
					value = v
					draw()
				}
			}
		case xproto.FocusOutEvent:
			// Grabs by the window manager or the server move the focus for a
			// moment and give it back; only a real loss counts.
			if e.Mode != xproto.NotifyModeNormal {
				continue
			}
			note(log, "focus lost at "+time.Now().Format(time.RFC3339Nano))
			focus()
		}
	}
}

// secret owns the CLIPBOARD selection with probe.SecretText and the type
// x-kde-passwordManagerHint, as KeePassXC offers a password, and appends
// "ready" to the log once it owns it. It serves the text, the type and the
// list of types until another program takes the selection.
func secret(log string) error {
	conn, err := xgb.NewConn()
	if err != nil {
		return err
	}
	defer conn.Close()
	atom := func(name string) (xproto.Atom, error) {
		reply, err := xproto.InternAtom(conn, false, uint16(len(name)), name).Reply()
		if err != nil {
			return 0, err
		}
		return reply.Atom, nil
	}
	atoms := map[string]xproto.Atom{}
	for _, name := range []string{"CLIPBOARD", "TARGETS", "UTF8_STRING", "x-kde-passwordManagerHint"} {
		if atoms[name], err = atom(name); err != nil {
			return err
		}
	}
	screen := xproto.Setup(conn).DefaultScreen(conn)
	owner, err := xproto.NewWindowId(conn)
	if err != nil {
		return err
	}
	if err := xproto.CreateWindowChecked(conn, 0, owner, screen.Root, 0, 0, 1, 1, 0,
		xproto.WindowClassInputOnly, 0, 0, nil).Check(); err != nil {
		return err
	}
	xproto.SetSelectionOwner(conn, owner, atoms["CLIPBOARD"], xproto.TimeCurrentTime)
	reply, err := xproto.GetSelectionOwner(conn, atoms["CLIPBOARD"]).Reply()
	if err != nil {
		return err
	}
	if reply.Owner != owner {
		return fmt.Errorf("another program kept the clipboard")
	}
	note(log, "ready")
	for {
		event, err := conn.WaitForEvent()
		if err != nil {
			continue
		}
		switch e := event.(type) {
		case nil:
			return fmt.Errorf("the X server closed the connection")
		case xproto.SelectionClearEvent:
			return nil
		case xproto.SelectionRequestEvent:
			property := e.Property
			switch e.Target {
			case atoms["TARGETS"]:
				targets := []xproto.Atom{atoms["TARGETS"], atoms["UTF8_STRING"], atoms["x-kde-passwordManagerHint"]}
				data := make([]byte, 4*len(targets))
				for i, target := range targets {
					xgb.Put32(data[4*i:], uint32(target))
				}
				xproto.ChangeProperty(conn, xproto.PropModeReplace, e.Requestor, property, xproto.AtomAtom, 32, uint32(len(targets)), data)
			case atoms["UTF8_STRING"], atoms["x-kde-passwordManagerHint"]:
				text := probe.SecretText
				if e.Target != atoms["UTF8_STRING"] {
					text = "secret"
				}
				xproto.ChangeProperty(conn, xproto.PropModeReplace, e.Requestor, property, atoms["UTF8_STRING"], 8, uint32(len(text)), []byte(text))
			default:
				property = xproto.AtomNone
			}
			notify := xproto.SelectionNotifyEvent{
				Time: e.Time, Requestor: e.Requestor, Selection: e.Selection, Target: e.Target, Property: property,
			}
			xproto.SendEvent(conn, false, e.Requestor, 0, string(notify.Bytes()))
		}
	}
}

// note appends a line to a window's log.
func note(log, line string) {
	if f, err := os.OpenFile(log, os.O_APPEND|os.O_CREATE|os.O_WRONLY, 0o644); err == nil {
		fmt.Fprintln(f, line)
		f.Close()
	}
}

// keyboardMap is the server's table from keycodes to keysyms: perCode
// keysyms for each keycode from minCode on, the unshifted one first.
func keyboardMap(conn *xgb.Conn) ([]xproto.Keysym, byte, xproto.Keycode, error) {
	setup := xproto.Setup(conn)
	reply, err := xproto.GetKeyboardMapping(conn, setup.MinKeycode, byte(setup.MaxKeycode-setup.MinKeycode+1)).Reply()
	if err != nil {
		return nil, 0, 0, err
	}
	return reply.Keysyms, reply.KeysymsPerKeycode, setup.MinKeycode, nil
}
