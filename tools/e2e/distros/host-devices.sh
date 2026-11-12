#!/bin/sh
# Gives tester what the user of a desktop session has on its devices.
#
# On a desktop, logind's uaccess rule gives the user of the active session an
# ACL on the GPU nodes and on the KVM's USB device. A container has no seat,
# so this loop sets those ACLs: on /dev/dri, and on each USB device with the
# OS Services interface (ff/4a/01). It sets them again after udev changes a
# node or the KVM enumerates again.
set -u

allow() {
    getfacl -p "$1" 2>/dev/null | grep -q '^user:tester:rw' || setfacl -m u:tester:rw "$1"
}

while :; do
    for node in /dev/dri/card* /dev/dri/renderD*; do
        [ -c "$node" ] && allow "$node"
    done
    for interface in /sys/bus/usb/devices/*:*; do
        [ "$(cat "$interface/bInterfaceClass" 2>/dev/null)" = ff ] || continue
        [ "$(cat "$interface/bInterfaceSubClass" 2>/dev/null)" = 4a ] || continue
        device=$(dirname "$(readlink -f "$interface")")
        allow "$(printf '/dev/bus/usb/%03d/%03d' "$(cat "$device/busnum")" "$(cat "$device/devnum")")"
    done
    sleep 1
done
