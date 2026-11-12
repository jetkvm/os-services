# Ubuntu 24.04 with an Xfce session on an X server on :99 on the host's display, and the Clipman
# clipboard manager, as Xubuntu has them.
FROM ubuntu:24.04
COPY base.sh host-devices.sh /usr/local/sbin/
RUN sh /usr/local/sbin/base.sh
RUN apt-get update && apt-get install -y --no-install-recommends xserver-xorg-core xserver-xorg-input-libinput libgl1-mesa-dri x11-xserver-utils \
        xfce4-session xfwm4 xfce4-panel xfdesktop4 xfce4-settings xfconf xfce4-clipman thunar dbus-x11 && \
    rm -rf /var/lib/apt/lists/*
COPY xorg-desktop.service /etc/systemd/system/e2e-desktop.service
COPY xfce-session.conf /etc/systemd/system/e2e-desktop.service.d/session.conf
COPY --chown=tester:tester <<EOF /home/tester/.config/systemd/user/xfce4-session.service
[Unit]
Description=Xfce session

[Service]
ExecStart=/usr/bin/xfce4-session
EOF
COPY --chown=tester:tester <<EOF /home/tester/.config/environment.d/display.conf
DISPLAY=:99
XDG_SESSION_TYPE=x11
XDG_CURRENT_DESKTOP=XFCE
EOF
# Clipman's autostart entry is hidden until the user turns it on in the
# session settings, which write this file.
RUN cd /home/tester/.config && \
    mkdir autostart && \
    sed 's/^Hidden=true/Hidden=false/' /etc/xdg/autostart/xfce4-clipman-plugin-autostart.desktop \
        > autostart/xfce4-clipman-plugin-autostart.desktop && \
    chown -R tester:tester /home/tester/.config
STOPSIGNAL SIGRTMIN+3
CMD ["/sbin/init"]
