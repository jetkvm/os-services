# e2e: test a build on real computers

`e2e` builds the checkout that it runs from, with uncommitted changes. Then it installs that build on several computers and tests it through a real KVM. For each computer, it prints one result line: passed, or the first check that failed.

[perf](../perf/README.md) uses the same lab file to measure the video pipeline of the same build.

To run `e2e` from the repository root:

```sh
cargo xtask e2e -- lab.json
```

To run it from `tools/`, with a subset of the targets or checks:

```sh
go run ./e2e [-targets mac/mini-mac,ubuntu/mini-devbox] [-checks files,video] lab.json
```

## How a run works

1. `e2e` builds the installer for each operating system that a target uses. The builds run at the same time. See [Builds](#builds).
2. For each target in the lab file, in order, `e2e` does these steps:
   1. It connects the USB of the KVM to the computer, by cable or over USB/IP.
   2. It connects to the KVM over WebRTC, as the web page of the KVM does. A JetKVM has one session at a time, so this ends the session of a web page that is open on the KVM.
   3. It copies the installer to the computer over SSH, installs it, approves the key of the KVM, and starts the service.
   4. It runs the checks. The first check that fails ends the target.
   5. It restores the OS Services settings of the KVM, stops the service, and disconnects the USB.

## Checks

| Check | What it checks |
| --- | --- |
| `session` | The service opens an approved session and reports its version. This check always runs first. |
| `clipboard` | Text that the KVM writes to the clipboard of the computer reads back the same. The text includes characters that are not ASCII. |
| `clipboard-secret` | Text that a password manager marks as secret stays on the computer: the KVM's read gets "denied" and no text. A helper in the desktop session puts the text on the clipboard with the mark that password managers use: the type `x-kde-passwordManagerHint` on X11 and Xwayland, `org.nspasteboard.ConcealedType` on macOS, and the format `ExcludeClipboardContentFromMonitorProcessing` on Windows. |
| `files` | A dropped file of 3 MiB arrives with the same SHA-256 hash. The check then removes the file. |
| `video` | The screen of the computer arrives as H.264 video that decodes without errors, at the size that the KVM reports. After the recording, the KVM still shows the video of the service, not its own capture. |
| `forget` | After `--forget-kvm` removes the KVM from the approved list, the running service ends the session in 5 seconds or less and opens no new one. After `--approve-kvm`, the session opens again. |
| `reconnect` | After the service stops and starts again, the session opens again. |

Before the `video` check, `e2e` turns video off, waits until the KVM shows its own video, and turns video on again. The KVM allows three video switches in two minutes, also across sessions (see the [switch limit](../../PROTOCOL.md#switch-to-service-video)). The user action of turning video off and on resets this limit, so earlier targets on the same KVM do not block the check.

## Lab file

The lab file describes your lab: the builds, the KVMs, the computers and the targets. A target is one computer with one KVM. The file contains addresses and passwords, so keep it outside the repository.

```json
{
  "build": {
    "linux":   { "docker": true },
    "windows": { "ssh": "builder@win-builder", "dir": "C:/build/os-services" },
    "macos":   { "ssh": "builder@192.0.2.20", "dir": "~/build/os-services",
                 "signing_identity": "OS Services Dev",
                 "keychain": "~/Library/Keychains/os-services-e2e.keychain-db",
                 "keychain_password": "…" }
  },
  "kvms": {
    "mini-mac":    { "url": "http://192.0.2.31" },
    "mini-devbox": { "url": "http://192.0.2.32",
                     "usbip": { "hub": "tester@192.0.2.40", "busid": "1-3" } },
    "jetkvm":      { "type": "jetkvm", "url": "http://192.0.2.33" }
  },
  "computers": {
    "mac":    { "ssh": "tester@192.0.2.20", "os": "macos" },
    "n100":   { "ssh": "tester@192.0.2.21", "os": "linux", "own_kvm": { "usb_port": "1-2" } },
    "ubuntu": { "ssh": "tester@127.0.0.1:2201", "os": "linux" }
  },
  "targets": [
    { "computer": "n100",   "kvm": "mini-devbox", "link": "usbip" },
    { "computer": "ubuntu", "kvm": "mini-devbox" },
    { "computer": "ubuntu", "kvm": "jetkvm" },
    { "computer": "mac",    "kvm": "mini-mac" }
  ]
}
```

- `kvms.<name>`: a KVM.
- `kvms.<name>.type`: `mini` for a JetKVM Mini, or `jetkvm` for a JetKVM. If it is not set, the KVM is a JetKVM Mini.
- `kvms.<name>.password`: the web password of the KVM. If it is not set, `e2e` uses `JETKVM_PASSWORD`. A JetKVM gets the password only if it asks for one, because a login to a JetKVM signs out the web pages that use it.
- `kvms.<name>.usbip`: the hub that shares the USB of the KVM. See [USB over the network](#usb-over-the-network).
- `computers.<name>.own_kvm`: a KVM that is connected to the computer itself. During a USB/IP target, `e2e` switches this KVM off, so that the service uses the KVM over the network. After the target, `e2e` switches it on again. Give one of these:
  - `usb_port`: the Linux USB port, as in `/sys/bus/usb/devices`.
  - `windows_device`: on Windows, the device instance ID of the KVM's OS Services interface, as `pnputil /enum-devices /connected` shows it. For example: `USB\\VID_399B&PID_0001&MI_06\\6&43EEFAA&0&0006` in the lab file. The keyboard and the mouse of the KVM continue to work.
  - For a Proxmox VM: `proxmox` (the SSH login of the Proxmox host), `vm`, and `usb` (the USB option of the VM, for example `usb0`).
- `computers.<name>.skip`: the checks that cannot run on the computer, for example `["clipboard-secret"]`. The result line of the target names them. The `session` check always runs.
- `targets[].link`: `usbip` for a KVM over the network. For a cable, do not set it.

Put the USB/IP targets first and a cable target last. The last target then checks a real cable.

## Builds

`e2e` builds the files on disk that git tracks or does not ignore. Each build writes its log to `target/e2e-build/<system>.log`.

- `docker`: builds Linux on this computer with the `Dockerfile` of the repository, as CI does.
- `ssh` and `dir`: sends the tree to a build machine, runs `cargo xtask setup` and `cargo xtask package` there with the environment of CI, and gets `dist/`. The folder keeps its `target/` between builds, so a second build takes approximately one minute.
- `dir` is a folder only for builds. Each build deletes the contents of the folder, except `target/`. A build starts only in a new or empty folder, or in a folder with the file `.os-services-e2e-build`, which the first build writes. A build does not start in the home folder, in a folder that contains the home folder, or in a path with `..`.
- `signing_identity`: on macOS, the identity that signs the app. The Screen Recording permission stays only while the signature stays the same. Use the same identity for each build, and give the permission one time.
- `keychain` and `keychain_password`: the keychain that holds the identity. An SSH session cannot use a keychain that a different session unlocked, so the build unlocks it. Use a keychain only for builds, not the login keychain.

## Requirements

On the computer that runs `e2e`:

- Go, `git`, `tar`, `ssh`, and FFmpeg (`ffmpeg` and `ffprobe`).
- Docker, for Linux builds.
- An SSH login to each build machine. Each build machine needs the build tools in [Develop IP-KVM OS Services](../../docs/development.md#set-up-the-build).
- An IPv4 route to each KVM.

On each computer under test:

- An SSH login without a password prompt (`user@host` or `user@host:port`), with your own key or the lab key. See [Linux distributions in containers](distros/README.md).
- A desktop session of the SSH user.
- On Linux: passwordless `sudo` for `--install-linux-access`.
- On macOS: the user is signed in at the screen.
- On Windows: `sh` from Git for Windows is the shell of the SSH login.

## Linux distributions in containers

`distros/` starts Linux distributions in containers, as computers under test. See [Linux distributions in containers](distros/README.md).

## USB over the network

One KVM can serve several computers. The KVM is connected to a hub computer, and the computer under test attaches the KVM over USB/IP:

```json
"usbip": { "hub": "tester@192.0.2.40", "busid": "1-2", "attach_on": "tester@192.0.2.41" }
```

- `hub`: the SSH login of the computer that the KVM is connected to.
- `busid`: the USB port of the KVM on the hub, as `usbip list -l` shows it on the hub.
- `attach_on`: the computer whose kernel attaches the KVM. If it is not set, `e2e` uses the computer under test. For a computer under test that is a container, use the computer that runs the container.

The hub needs Linux, `usbip` (the `usbip` package on Debian and Ubuntu) and passwordless `sudo`. The hub must not run OS Services.

The attaching computer runs one of these systems:

- Linux, with `usbip` and passwordless `sudo`. A loop attaches the KVM again each time the KVM enumerates again, for example after a restart or a media mount.
- Windows, with [usbip-win2](https://github.com/vadimgrn/usbip-win2) in `C:\Program Files\USBip` and an SSH login that is an administrator. Its `attach` command attaches the KVM again automatically.

macOS has no USB/IP client, so a Mac uses a cable.

## Layout

| Folder | Contents |
| --- | --- |
| `kvm/` | The KVM client. It uses JSON-RPC on the `rpc` data channel, as the web page of the KVM does. `signaling.go` is the WebSocket signaling of the web page, the same for a JetKVM Mini and a JetKVM. |
| `link/` | The `Link` interface, with `Cable` and `USBIP`. |
| `build/` | The builds of the checkout. |
| `distros/` | The containers of Linux distributions. |
| `computer/` | The computer under test over SSH, with the commands of each operating system. |
| `ssh/` | Runs scripts over SSH with your keys and the lab key. |
| `check/` | The checks. |
| `lab/` | The lab file, the builds, and the run of one target: connect, install, start the desktop and the service, and restore everything. `perf` also uses it. |
| `probe/` | The layout of the noise window and the key window of `perf`, and `probe/x11`, which shows both windows on X11. |
