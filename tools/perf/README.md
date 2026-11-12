# perf: measure the video pipeline of a build

`perf` measures one build of OS Services on the computers of a lab file, one computer at a time, and prints the results. It builds the checkout that it runs from, with uncommitted changes. It uses the lab file, the builds and the computers of [e2e](../e2e/README.md). `perf` fails a target only when it cannot measure it.

To run `perf` from the repository root:

```sh
cargo xtask perf -- lab.json
```

To run it from `tools/`, with a subset of the targets:

```sh
go run ./perf [-targets mac/mini-mac,ubuntu/mini-devbox] lab.json
```

## What it measures

`perf` measures only the targets whose KVM is connected by cable. Over USB/IP, the network and the hub are part of each result, so `perf` skips those targets.

`perf` measures each target at 1920x1080, so that you can compare the targets. It sets the OS Services resolution of the KVM, and restores it after the target.

For each target, `perf` prints three lines:

```text
[mac/mini-mac] 1920x1080 59.3 fps 4043 kbit/s, captured 60.0 fps, cap 6.30/7.90 ms, conv 0.21/0.25 ms, enc 6.77/7.02 ms, ...
[mac/mini-mac] key to video p50 39.1 ms, p95 54.2 ms (60 of 60 keys)
[mac/mini-mac] key to frame at the KVM p50 33.4 ms, p95 47.9 ms (60 of 60 keys)
```

### The pipeline line

The first line shows the frame rate and the stage times under full load:

1. A noise window changes a grid of gray blocks in the left half of the screen every 4 ms. The screen changes at each refresh. For this reason, the service captures and encodes at the full rate, with the same load on each system.
2. `perf` records the video for 15 seconds.
3. The line shows the frame rate and the bitrate at the KVM, for the time from the first frame of the recording to its end. From the log of the service, it also shows the captured frames per second, and the p50 and p95 time of each stage of the pipeline. `perf` reads the log before the noise window closes.

### The key-to-video line

The second line shows the time from a key press to the video that shows it, on the clock of the computer that runs `perf`:

1. The noise window closes. `perf` moves the pointer of the KVM to the middle of the screen, opens a key window on the right of the screen, and clicks into it.
2. The KVM types 60 letters, with approximately 100 ms between letters. The key window shows five blocks, each black or white, that encode the last letter.
3. For each letter, `perf` records two times: when it sends the key press to the KVM, and when the first frame arrives whose blocks show that letter.

Both times come from the clock of the computer that runs `perf`, so no clock synchronization is necessary. The result includes the full path of a key press: the USB keyboard of the KVM, the redraw of the window, capture, encoding, the KVM and WebRTC.

`perf` does not count a letter if the next letter replaces it before a frame shows it. It also cannot time the letter after a letter that no frame showed. If no frame shows a letter, `perf` fails the target. The key window takes the focus back when it loses it, and the line shows how many times that occurred.

`perf` moves the pointer before the click because the kernel reports an absolute position only when the position changes. A desktop that started after the last report of the KVM keeps its pointer in a different position.

### The key-to-frame-at-the-KVM line

The third line shows the same keys on the clock of the KVM, without the network:

1. Before the recording, `perf` starts the timing log of the KVM (`GET /webrtc/stats?timing=start`). The KVM then logs two times of its own clock: when it wrote each keyboard report to USB, and when it had the last byte of each video frame from the service. It logs each frame with the RTP timestamp of its packets.
2. After the recording, `perf` reads the log. It finds the frames that show each letter in the recording, as for the second line, and finds each of them in the log by its RTP timestamp.
3. For each letter, the result is the time of the frame minus the time of the key report.

The result includes the USB keyboard, the redraw of the window, capture, encoding, and the USB transfer of the frame to the KVM. It does not include the network, or the time that the KVM needs to send the frame. The difference between the second line and the third line is that time and the network.

If the firmware of the KVM has no timing log, or the log does not have each key and each frame of the recording, the line tells why there is no result.

### The network

The network between the computer that runs `perf` and the KVM is part of the frame rate, the bitrate and the key-to-video time. It is not part of the key-to-frame-at-the-KVM time. As a browser does, `perf` asks the KVM again for lost packets. A frame with a lost packet waits for that packet for a maximum of 300 ms.

If packets did not arrive, also not as a retransmission, the line ends with the count, for example `lost 764 of 11348 packets (6.7%)`. A frame behind a lost packet arrives late or does not arrive. Such a line shows the network as much as the computer. Compare only runs from the same place on the network.

## The windows

`tools/e2e/probe` contains the layout of the windows. Both windows open on the screen that has the EDID of the KVM, which the service captures. `perf` waits until a window reports that it shows. On Windows, this takes some seconds, because the script of the window first compiles its C#. If a window finds no such screen, or more than one, it does not show, and `perf` fails the target.

Each system has a small program for the windows:

- **Linux:** `tools/e2e/probe/x11`, for X11 and Xwayland.
  - It finds the screen by the EDID of its RandR output. Xwayland gives its outputs no EDID, so on Xwayland it uses the only connected screen.
  - The key window is a normal window, because a Wayland compositor gives the keyboard focus only to a window that it manages and that the user clicked. A window manager can put the window in a different position or below a title bar. For this reason, the window writes the position of its content after "ready", and again after each move. It writes the position on the screen of the KVM, where `perf` reads the blocks. It also writes the position on the full desktop, where `perf` clicks, because the absolute pointer of the KVM covers the full desktop on X11.
  - `perf` builds it as a static program and copies it to the computer. It runs as a unit of the systemd user manager of the user, which has the variables of the desktop.
- **Windows:** `tools/e2e/computer/noise_windows.ps1` and `keys_windows.ps1`. They run as interactive tasks in the signed-in session, and find the screen by the PnP ID of the monitor.
- **macOS:** `tools/e2e/computer/probe_macos.swift`. The Mac compiles it when it changes, and starts it as a launch agent in the session that has the screen. It finds the screen by the vendor number of the display.

## Limits

- The letters need a QWERTY-style keyboard layout, where the key codes of the KVM give the letters a to z.
- On Windows, the pointer of the KVM moves on the primary screen. Make the screen of the KVM the primary screen, or the click does not hit the key window.
