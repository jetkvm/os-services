# Linux distributions in containers

`compose.yaml` starts Linux distributions in containers, as computers under test for [e2e](../README.md) and [perf](../../perf/README.md). Run the containers on the computer that the KVM is connected to, by USB and HDMI. Each container has its own Dockerfile in this folder.

| Name | Image | Desktop session | SSH port |
| --- | --- | --- | --- |
| `ubuntu` | Ubuntu 24.04 | X server | 2201 |
| `ubuntu-22` | Ubuntu 22.04 | X server | 2208 |
| `debian` | Debian 12 | X server | 2202 |
| `fedora` | Fedora 42 | X server | 2203 |
| `arch` | Arch Linux | X server | 2204 |
| `ubuntu-xfce` | Ubuntu 24.04 | Xfce on X11, with the Clipman clipboard manager | 2205 |
| `arch-kde` | Arch Linux | Plasma 6 on Wayland, with Klipper | 2206 |
| `debian-xfce` | Debian 13 | Xfce on X11, with the Clipman clipboard manager | 2207 |
| `arch-sway` | Arch Linux | sway on Wayland, with no desktop environment | 2209 |
| `devuan` | Devuan 6, which has no systemd | X server | 2211 |

## Start the containers

Run these commands from `tools/`:

```sh
go run ./e2e -lab-key                                               # one time
docker compose -f e2e/distros/compose.yaml up -d --build            # all containers
docker compose -f e2e/distros/compose.yaml up -d --build arch-kde   # or some of them
docker compose -f e2e/distros/compose.yaml down
```

In the lab file, a container is a computer with the SSH port from the table:

```json
"fedora": { "ssh": "tester@127.0.0.1:2203", "os": "linux" }
```

`arch-sway` leaves out the `clipboard-secret` check. The helper of that check is an X11 program, and under sway an X11 program without a focused window does not set the Wayland clipboard:

```json
"arch-sway": { "ssh": "tester@127.0.0.1:2209", "os": "linux", "skip": ["clipboard-secret"] }
```

Keep the console of the host free of a desktop. The desktops of the containers use the display of the host.

## The lab key

The containers trust only the lab key. `e2e` creates this SSH key in `~/.local/state/os-services-e2e/lab-key/`, or under `$XDG_STATE_HOME` when that variable is set. `-lab-key` creates the key and prints the file of its public half. Compose mounts that folder read-only, so no personal key goes into a container.

`e2e` offers the lab key after your own keys: the keys of the SSH agent and the default key files of ssh. Each computer accepts the key that it trusts, so a computer of your own needs no change.

sshd creates the host key of each container at its first start, in a volume. The host key stays the same when you build the image again.

## What a container contains

Each container is privileged and runs systemd, sshd, and the user `tester` with a user manager. `base.sh` sets up this common part.

The user manager has the variables of the desktop (`DISPLAY`, `WAYLAND_DISPLAY`, `XDG_CURRENT_DESKTOP`). For this reason, the clipboard check goes through that desktop: X11 selections on Xfce and the X server, and the Wayland data-control protocol on KDE.

The container has the GPU and the USB devices of the host, so the KVM connects to it by cable. The image contains what a desktop of that distribution has for OS Services, from the packages of that distribution: the libraries that the AppImage does not include, and the Mesa VAAPI drivers of the distribution. All containers encode with the VAAPI drivers of their distribution. Fedora encodes with OpenH264.

The rest is the real install: the AppImage, `--install-linux-access` with udev and `setcap`, and the systemd user unit.

`devuan` has no systemd. Its first process is `e2e-init` (`no-systemd-init.sh`), which runs udev, the device ACLs and sshd. The service has no unit there: the installed program keeps the service running, and `e2e` starts it with `--install-service`, stops it with `pkill`, and reads its log file. A login over SSH gets `DISPLAY` from `/etc/environment`, so the service and the windows of `perf` have it.

## The desktop

The desktop of each container runs on the display of the host. The KVM captures this display over HDMI, as it does for a desktop on a computer of its own. Only one container can use the display at a time. For this reason, the desktop does not start at boot. `e2e` starts `e2e-desktop.service` in the container after the install, and stops it at the end of the target.

Each desktop runs in a logind session of `tester` on `tty2`, as a display manager starts it. The logind of the container then gives the desktop the GPU and the input devices. logind also keeps the access of `tester` to the GPU when udev changes it, for example when the EDID of the KVM changes. logind gives a session the GPU only when the session starts on the console in front, so the unit changes to `tty2` first.

- In the X server and Xfce containers, `e2e-desktop.service` (`xorg-desktop.service`) runs Xorg without root. Without Xfce, the X server shows a blue background and a pointer.
  - Xorg does not turn on a screen that appears after Xorg starts. The screen of the KVM disconnects for a short time each time the KVM changes its EDID. For this reason, `xorg-screen.conf` starts Xorg only when a screen is connected.
  - The Xfce containers start their session on Xorg with `xfce-session.conf`.
- In `arch-kde`, the unit runs KWin on the GPU, with Xwayland and the Plasma shell. The shell starts only after the user manager gets the `DISPLAY` of Xwayland, as in the startup of Plasma. For this reason, X11 programs that run as user units, such as the windows of `perf`, connect to Xwayland.
  - The unit is started when Klipper, which runs in the Plasma shell, is on the session bus. From then on, Klipper sees each change of the clipboard.
  - Klipper starts with an empty history (`klipperrc`). At its start, Klipper puts the top entry of a saved history on the clipboard, in place of what a program put there in the meantime.
- In `arch-sway`, the unit runs sway on the GPU, with Xwayland. The configuration is the one of the Arch package, which gives the user manager the variables of the session.
- In `devuan`, `e2e` runs the script `e2e-desktop` (`no-systemd-desktop.sh`) in place of the unit. The script runs Xorg as root, because the container has no logind that gives a user the GPU.

## Input and device access

The desktops get the keyboard and the mouse of the KVM from the input devices of the host. Docker copies `/dev` only when a container starts, and the devices of the KVM appear again each time the USB of the KVM enumerates again. For this reason, Compose binds the `/dev/input` folder of the host into the containers. Each container runs its own udev, which receives the device events of the kernel.

The logind of a desktop gives the session user an ACL on the GPU and on the KVM (`uaccess`). A container has no seat, so `host-devices.sh` sets these ACLs.

After `--install-linux-access`, the install starts the user manager of `tester` again, because the service needs the new `ipkvm-capture` group. On a real desktop, the install stops and asks you to sign in again.

## Add a desktop

1. Copy the Dockerfile that is the most similar.
2. Change its packages and its session units.
3. Add a service with a free SSH port to `compose.yaml`.
