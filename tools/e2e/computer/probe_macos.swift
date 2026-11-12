// perf's windows on macOS (see tools/e2e/probe), on the screen whose
// display has the KVM's EDID vendor, above other windows:
//
//	probe noise left top width height columns rows interval-ms window.log
//	probe keys left top width height window.log
//	probe secret text window.log
//
// Each window appends "ready" to window.log once it shows. The noise window
// redraws a grid of gray blocks every interval with one of 60 frames made at
// the start. The key window's five blocks show the last letter typed (a is
// 1, z is 26, the first block is the highest bit, white is 1); it takes the
// keyboard focus back when it loses it, and appends a line to window.log
// each time it lost the focus.
import AppKit

let args = CommandLine.arguments
func usage() -> Never {
    FileHandle.standardError.write("usage: probe noise left top width height columns rows interval-ms window.log | probe keys left top width height window.log | probe secret text window.log\n".data(using: .utf8)!)
    exit(2)
}
guard args.count >= 4 else { usage() }
let log = args[args.count - 1]

// note appends a line to the window's log.
func note(_ line: String) {
    if !FileManager.default.fileExists(atPath: log) {
        FileManager.default.createFile(atPath: log, contents: nil)
    }
    if let handle = FileHandle(forWritingAtPath: log) {
        handle.seekToEndOfFile()
        handle.write((line + "\n").data(using: .utf8)!)
        handle.closeFile()
    }
}

// The secret helper puts the text on the pasteboard with the type
// org.nspasteboard.ConcealedType, as password managers mark a password, and
// runs until it is stopped.
if args[1] == "secret" {
    guard args.count == 4 else { usage() }
    let concealed = NSPasteboard.PasteboardType("org.nspasteboard.ConcealedType")
    let pasteboard = NSPasteboard.general
    pasteboard.declareTypes([.string, concealed], owner: nil)
    pasteboard.setString(args[2], forType: .string)
    pasteboard.setString("", forType: concealed)
    note("ready")
    dispatchMain()
}

guard args.count >= 7, let left = Double(args[2]), let top = Double(args[3]),
      let width = Double(args[4]), let height = Double(args[5]) else { usage() }

final class NoiseView: NSView {
    let columns: Int, rows: Int
    var frames: [[CGFloat]] = []
    var next = 0

    init(frame: NSRect, columns: Int, rows: Int) {
        self.columns = columns
        self.rows = rows
        frames = (0..<60).map { _ in (0..<(columns * rows)).map { _ in CGFloat(Int.random(in: 0..<16)) / 15 } }
        super.init(frame: frame)
    }
    required init?(coder: NSCoder) { fatalError() }

    override func draw(_ dirtyRect: NSRect) {
        let grays = frames[next % frames.count]
        for row in 0..<rows {
            for column in 0..<columns {
                NSColor(white: grays[row * columns + column], alpha: 1).setFill()
                let x0 = bounds.width * CGFloat(column) / CGFloat(columns), x1 = bounds.width * CGFloat(column + 1) / CGFloat(columns)
                let y0 = bounds.height * CGFloat(row) / CGFloat(rows), y1 = bounds.height * CGFloat(row + 1) / CGFloat(rows)
                NSRect(x: x0, y: y0, width: x1 - x0, height: y1 - y0).fill()
            }
        }
    }
}

final class BlocksView: NSView {
    var value = 0

    override func draw(_ dirtyRect: NSRect) {
        NSColor(white: 0.5, alpha: 1).setFill()
        bounds.fill()
        let side = 0.12 * bounds.width
        for i in 0..<5 {
            let cx = (0.1 + 0.2 * CGFloat(i)) * bounds.width, cy = bounds.height / 2
            ((value >> (4 - i)) & 1 == 1 ? NSColor.white : NSColor.black).setFill()
            NSRect(x: cx - side / 2, y: cy - side / 2, width: side, height: side).fill()
        }
    }
}

// A borderless window takes the keyboard only when it says it can.
final class KeyWindow: NSWindow {
    override var canBecomeKey: Bool { true }

    override func keyDown(with event: NSEvent) {
        guard let c = event.charactersIgnoringModifiers?.lowercased().unicodeScalars.first,
              c.value >= 0x61, c.value <= 0x7a, let view = contentView as? BlocksView else { return }
        view.value = Int(c.value) - 0x60
        view.display()
    }
}

final class FocusKeeper: NSObject, NSWindowDelegate {
    func windowDidResignKey(_ notification: Notification) {
        note("focus lost at \(ISO8601DateFormatter().string(from: Date()))")
        DispatchQueue.main.async {
            NSApp.activate(ignoringOtherApps: true)
            (notification.object as? NSWindow)?.makeKeyAndOrderFront(nil)
        }
    }
}

let app = NSApplication.shared
app.setActivationPolicy(.regular)
// The service captures the display with the KVM's EDID vendor ("JET").
let kvmScreens = NSScreen.screens.filter { screen in
    guard let id = screen.deviceDescription[NSDeviceDescriptionKey("NSScreenNumber")] as? NSNumber else { return false }
    return CGDisplayVendorNumber(CGDirectDisplayID(id.uint32Value)) == 0x28b4
}
guard kvmScreens.count == 1 else {
    FileHandle.standardError.write("\(kvmScreens.count) screens show the KVM's EDID, not one\n".data(using: .utf8)!)
    exit(1)
}
let screen = kvmScreens[0].frame
// AppKit counts y from the bottom of the screen.
let frame = NSRect(x: screen.minX + left * screen.width, y: screen.minY + (1 - top - height) * screen.height,
                   width: width * screen.width, height: height * screen.height)
var keeper: FocusKeeper?
var timer: Timer?

switch args[1] {
case "noise":
    guard args.count == 10, let columns = Int(args[6]), let rows = Int(args[7]), let interval = Double(args[8]) else { usage() }
    // An accessory app takes no focus and shows no Dock icon.
    app.setActivationPolicy(.accessory)
    let window = NSWindow(contentRect: frame, styleMask: .borderless, backing: .buffered, defer: false)
    window.level = .floating
    let view = NoiseView(frame: NSRect(origin: .zero, size: frame.size), columns: columns, rows: rows)
    window.contentView = view
    window.orderFrontRegardless()
    note("ready")
    timer = Timer.scheduledTimer(withTimeInterval: interval / 1000, repeats: true) { _ in
        view.next += 1
        view.display()
    }
case "keys":
    guard args.count == 7 else { usage() }
    let window = KeyWindow(contentRect: frame, styleMask: .borderless, backing: .buffered, defer: false)
    window.level = .floating
    window.contentView = BlocksView(frame: NSRect(origin: .zero, size: frame.size))
    keeper = FocusKeeper()
    window.delegate = keeper
    window.makeKeyAndOrderFront(nil)
    app.activate(ignoringOtherApps: true)
    note("ready")
default:
    usage()
}
app.run()
