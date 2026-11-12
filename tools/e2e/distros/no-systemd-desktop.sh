#!/bin/sh
# The desktop of a container without systemd, on the host's display: an X
# server on :99 that drives the host's GPU output, which the KVM captures.
# tools/e2e runs "e2e-desktop start" for a target and "e2e-desktop stop"
# after it, as it starts and stops e2e-desktop.service elsewhere.
#
# The X server runs as root, as on a computer without logind, and accepts
# the connections of every local user (-ac). xorg-desktop.service explains
# the other options, and xorg-screen.conf the wait for a screen.
set -eu
pid=/run/e2e-desktop.pid

stop() {
    [ -f "$pid" ] || return 0
    kill "$(cat "$pid")" 2>/dev/null || true
    i=0
    while kill -0 "$(cat "$pid")" 2>/dev/null && [ $i -lt 100 ]; do
        i=$((i + 1))
        sleep 0.1
    done
    rm -f "$pid"
}

case "${1:-}" in
start)
    stop
    i=0
    until grep -qx connected /sys/class/drm/card*-*/status; do
        [ $i -lt 100 ] || { echo "no screen is connected" >&2; exit 1; }
        i=$((i + 1))
        sleep 0.1
    done
    chvt 2
    setsid Xorg :99 vt2 -nolisten tcp -novtswitch -noreset -s 0 -dpms -ac >/var/log/e2e-desktop.log 2>&1 &
    echo $! > "$pid"
    i=0
    until [ -S /tmp/.X11-unix/X99 ]; do
        [ $i -lt 100 ] || { echo "the X server did not start" >&2; exit 1; }
        i=$((i + 1))
        sleep 0.1
    done
    # Without a desktop, the root window is black and has no pointer.
    xsetroot -display :99 -solid "#3465a4" -cursor_name left_ptr
    ;;
stop)
    stop
    ;;
*)
    echo "usage: e2e-desktop start|stop" >&2
    exit 1
    ;;
esac
