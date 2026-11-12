#!/bin/sh
# The part that all test computers in compose.yaml share; each Dockerfile
# runs it, then adds its desktop session.
#
# The packages are what OS Services finds on a desktop: FUSE 2 for the
# AppImage; the libraries that the AppImage leaves to the system (libEGL,
# libGLES, libdrm); libva with the distribution's Mesa VAAPI
# drivers; setcap, sudo, curl, and chvt (Debian's kbd). Then systemd
# with the user "tester" and a user manager, sshd for the lab key, and the
# device ACLs of a desktop session.
set -eu

if command -v apt-get >/dev/null; then
    export DEBIAN_FRONTEND=noninteractive
    apt-get update
    fuse2=libfuse2
    apt-cache show libfuse2t64 >/dev/null 2>&1 && fuse2=libfuse2t64
    apt-get install -y --no-install-recommends systemd systemd-sysv dbus dbus-user-session udev \
        openssh-server sudo curl ca-certificates acl libcap2-bin fuse3 "$fuse2" \
        libegl1 libgles2 libdrm2 libva2 libva-drm2 mesa-va-drivers procps kbd
    rm -rf /var/lib/apt/lists/*
elif command -v dnf >/dev/null; then
    dnf install -y systemd dbus-daemon udev openssh-server sudo curl acl libcap fuse fuse-libs \
        libglvnd-egl libglvnd-gles mesa-libEGL libdrm libva mesa-va-drivers procps-ng findutils
    dnf clean all
elif command -v pacman >/dev/null; then
    pacman -Syu --noconfirm --needed systemd dbus openssh sudo curl acl libcap fuse2 fuse3 \
        libglvnd mesa libdrm libva procps-ng
    pacman -Scc --noconfirm
else
    echo "no known package manager" >&2
    exit 1
fi

# uid 1000, the uid of most desktop users. Some images already have a user
# with it (ubuntu on Ubuntu).
existing=$(getent passwd 1000 | cut -d: -f1)
[ -z "$existing" ] || userdel -r "$existing"
useradd -m -u 1000 -s /bin/bash tester
echo 'tester ALL=(ALL) NOPASSWD: ALL' > /etc/sudoers.d/tester
chmod 440 /etc/sudoers.d/tester
mkdir -p /var/lib/systemd/linger
touch /var/lib/systemd/linger/tester
mkdir -p /home/tester/.config/systemd/user/default.target.wants /home/tester/.config/environment.d
chown -R tester:tester /home/tester/.config

# SSH: the lab key only (compose.yaml mounts its folder at /etc/ssh/lab-key),
# and a host key that sshd creates at the first start, in a volume, so that
# it stays the same when the image is built again. ssh offers the agent's
# keys before the lab key, so sshd accepts more attempts than its default 6.
mkdir -p /etc/ssh/sshd_config.d
cat > /etc/ssh/sshd_config.d/e2e.conf <<'EOF'
PasswordAuthentication no
MaxAuthTries 30
HostKey /etc/ssh/host-key/ssh_host_ed25519_key
AuthorizedKeysFile /etc/ssh/lab-key/id_ed25519.pub
EOF
grep -q '^Include /etc/ssh/sshd_config.d' /etc/ssh/sshd_config ||
    sed -i '1i Include /etc/ssh/sshd_config.d/*.conf' /etc/ssh/sshd_config
cat > /etc/systemd/system/e2e-ssh-host-key.service <<'EOF'
[Unit]
Description=Create the SSH host key in its volume

[Service]
Type=oneshot
ExecStart=/bin/sh -c 'k=/etc/ssh/host-key/ssh_host_ed25519_key; [ -f $k ] || ssh-keygen -q -t ed25519 -N "" -f $k'
EOF
for unit in ssh.service sshd.service; do
    if systemctl enable "$unit" 2>/dev/null; then
        mkdir -p "/etc/systemd/system/$unit.d"
        printf '[Unit]\nRequires=e2e-ssh-host-key.service\nAfter=e2e-ssh-host-key.service\n' \
            > "/etc/systemd/system/$unit.d/e2e-host-key.conf"
        break
    fi
done

# The GPU and the KVM, as a desktop session has them.
cat > /etc/systemd/system/e2e-host-devices.service <<'EOF'
[Unit]
Description=Give tester the GPU and KVM devices, as uaccess does

[Service]
ExecStart=/bin/sh /usr/local/sbin/host-devices.sh

[Install]
WantedBy=multi-user.target
EOF
systemctl enable e2e-host-devices.service

# No login prompts or first-boot questions: a privileged container shares
# the host's consoles, and a getty on tty1 would draw on the screen that the
# KVM captures.
systemctl mask getty@.service autovt@.service serial-getty@.service console-getty.service \
    systemd-firstboot.service
