# Devuan 6, a Debian without systemd, with no desktop: an X server on :99 on
# the host's display. The service has no unit here; its keeper runs it. The
# first process is e2e-init, and e2e-desktop starts and stops the X server.
# A login over SSH gets DISPLAY from /etc/environment.
FROM devuan/devuan:excalibur
RUN apt-get update && apt-get install -y --no-install-recommends eudev openssh-server sudo curl ca-certificates \
        acl libcap2-bin fuse3 libegl1 libgles2 libdrm2 libva2 libva-drm2 mesa-va-drivers procps kbd \
        xserver-xorg-core xserver-xorg-input-libinput libgl1-mesa-dri x11-xserver-utils && \
    rm -rf /var/lib/apt/lists/*
COPY host-devices.sh /usr/local/sbin/
COPY --chmod=755 no-systemd-init.sh /usr/local/sbin/e2e-init
COPY --chmod=755 no-systemd-desktop.sh /usr/local/sbin/e2e-desktop
# The user and the SSH settings of base.sh.
RUN useradd -m -u 1000 -s /bin/bash tester && \
    echo 'tester ALL=(ALL) NOPASSWD: ALL' > /etc/sudoers.d/tester && chmod 440 /etc/sudoers.d/tester && \
    mkdir -p /etc/ssh/sshd_config.d && \
    printf '%s\n' 'PasswordAuthentication no' 'MaxAuthTries 30' \
        'HostKey /etc/ssh/host-key/ssh_host_ed25519_key' \
        'AuthorizedKeysFile /etc/ssh/lab-key/id_ed25519.pub' > /etc/ssh/sshd_config.d/e2e.conf && \
    echo DISPLAY=:99 >> /etc/environment
CMD ["/usr/local/sbin/e2e-init"]
