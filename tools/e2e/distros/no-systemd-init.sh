#!/bin/sh
# The first process of a container without systemd, in place of the units
# that base.sh sets up: the SSH host key in its volume, udev, the device
# ACLs of a desktop session, and sshd for the lab key.
set -eu
key=/etc/ssh/host-key/ssh_host_ed25519_key
[ -f "$key" ] || ssh-keygen -q -t ed25519 -N "" -f "$key"
mkdir -p /run/sshd
udevd --daemon
udevadm trigger --action=add
sh /usr/local/sbin/host-devices.sh &
exec /usr/sbin/sshd -D -e
