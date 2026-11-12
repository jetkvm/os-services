# Debian 12, with no desktop: an X server on :99 on the host's display.
FROM debian:12
COPY base.sh host-devices.sh /usr/local/sbin/
RUN sh /usr/local/sbin/base.sh
RUN apt-get update && apt-get install -y --no-install-recommends xserver-xorg-core xserver-xorg-input-libinput libgl1-mesa-dri x11-xserver-utils && \
    rm -rf /var/lib/apt/lists/*
COPY xorg-desktop.service /etc/systemd/system/e2e-desktop.service
COPY xorg-screen.conf /etc/systemd/system/e2e-desktop.service.d/screen.conf
RUN echo DISPLAY=:99 > /home/tester/.config/environment.d/display.conf
STOPSIGNAL SIGRTMIN+3
CMD ["/sbin/init"]
