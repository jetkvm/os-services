# Arch Linux with sway on the host's display: a Wayland window manager with
# no desktop environment, and Xwayland. sway starts neither
# graphical-session.target nor the autostart entries of a desktop.
FROM archlinux:latest
COPY base.sh host-devices.sh /usr/local/sbin/
RUN sh /usr/local/sbin/base.sh
RUN pacman -Sy --noconfirm --needed sway xorg-xwayland && pacman -Scc --noconfirm
# sway takes the GPU and the input devices from logind, as on a desktop, so
# it runs in a logind session of tester on tty2 (see arch-kde.Dockerfile).
# tools/e2e starts the unit for a target and stops it after; nothing starts
# it at boot. The unit is started when sway has its Wayland socket and the
# socket of Xwayland.
COPY <<EOF /etc/systemd/system/e2e-desktop.service
[Unit]
Description=sway on the host's display, in a logind session on tty2
After=systemd-user-sessions.service

[Service]
User=tester
PAMName=login
TTYPath=/dev/tty2
StandardInput=tty-force
StandardOutput=journal
StandardError=journal
Environment=XDG_SESSION_TYPE=wayland XDG_CURRENT_DESKTOP=sway
ExecStartPre=+/usr/bin/chvt 2
ExecStart=/usr/bin/sway
ExecStartPost=+/bin/sh -c 'i=0; until [ -S /run/user/1000/wayland-1 ] && [ -S /tmp/.X11-unix/X0 ]; do [ \$\$i -lt 300 ] || exit 1; i=\$\$((i + 1)); sleep 0.1; done'
EOF
# The sockets that sway opens. X11 programs that run as user units, such as
# the windows of perf, connect to Xwayland.
COPY --chown=tester:tester <<EOF /home/tester/.config/environment.d/display.conf
WAYLAND_DISPLAY=wayland-1
DISPLAY=:0
XDG_SESSION_TYPE=wayland
XDG_CURRENT_DESKTOP=sway
EOF
STOPSIGNAL SIGRTMIN+3
CMD ["/sbin/init"]
