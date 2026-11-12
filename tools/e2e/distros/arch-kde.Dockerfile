# Arch Linux with a Plasma 6 session on Wayland on the host's display: KWin
# with Xwayland, and plasmashell with Klipper.
FROM archlinux:latest
COPY base.sh host-devices.sh /usr/local/sbin/
RUN sh /usr/local/sbin/base.sh
RUN pacman -Sy --noconfirm --needed kwin plasma-workspace plasma-desktop xorg-xwayland && \
    pacman -Scc --noconfirm
# KWin takes the GPU and the input devices from logind, as on a desktop, so
# it runs in a logind session of tester on tty2. logind gives the session
# the DRM master only when the session starts on the console in front, so
# the unit brings tty2 to the front first. Commands with + run outside the
# session; otherwise each would open a session of its own on tty2. tools/e2e
# starts the unit for a target and stops it after; nothing starts it at
# boot.
#
# The unit is started when Klipper, in the Plasma shell, has its name on the
# session bus: from then on Klipper sees each change of the clipboard. The
# session bus accepts only its user, so the query runs as tester.
COPY <<EOF /etc/systemd/system/e2e-desktop.service
[Unit]
Description=Plasma on the host's display, in a logind session on tty2
After=systemd-user-sessions.service

[Service]
User=tester
PAMName=login
TTYPath=/dev/tty2
StandardInput=tty-force
StandardOutput=journal
StandardError=journal
Environment=XDG_SESSION_TYPE=wayland XDG_CURRENT_DESKTOP=KDE KDE_SESSION_VERSION=6
ExecStartPre=+/usr/bin/chvt 2
ExecStart=/usr/bin/kwin_wayland --drm --xwayland --socket wayland-0 /usr/local/bin/e2e-plasma
ExecStartPost=+/bin/sh -c 'i=0; until setpriv --reuid=tester --regid=tester --clear-groups busctl --address=unix:path=/run/user/1000/bus status org.kde.klipper >/dev/null 2>&1; do [ \$\$i -lt 300 ] || exit 1; i=\$\$((i + 1)); sleep 0.1; done'
EOF
# KWin starts Plasma once Xwayland runs. As in Plasma's own startup, the
# user manager gets Xwayland's DISPLAY first, so X11 programs started as
# user units reach it.
COPY --chmod=755 <<EOF /usr/local/bin/e2e-plasma
#!/bin/sh
systemctl --user import-environment DISPLAY
exec plasmashell
EOF
COPY --chown=tester:tester <<EOF /home/tester/.config/environment.d/display.conf
WAYLAND_DISPLAY=wayland-0
XDG_SESSION_TYPE=wayland
XDG_CURRENT_DESKTOP=KDE
KDE_SESSION_VERSION=6
QT_QPA_PLATFORM=wayland
EOF
# Klipper starts with an empty history. At its start, Klipper puts the top
# entry of a saved history on the clipboard, in place of what a program put
# there in the meantime.
COPY --chown=tester:tester <<EOF /home/tester/.config/klipperrc
[General]
KeepClipboardContents=false
EOF
STOPSIGNAL SIGRTMIN+3
CMD ["/sbin/init"]
