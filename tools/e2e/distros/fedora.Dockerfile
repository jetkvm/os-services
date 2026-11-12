# Fedora 42, with no desktop: an X server on :99 on the host's display.
FROM fedora:42
COPY base.sh host-devices.sh /usr/local/sbin/
RUN sh /usr/local/sbin/base.sh
# Fedora's Mesa has no H.264 encoder, so the service encodes with OpenH264
# from Fedora's Cisco repository.
RUN dnf install -y xorg-x11-server-Xorg xorg-x11-drv-libinput mesa-dri-drivers xsetroot openh264 && dnf clean all
COPY xorg-desktop.service /etc/systemd/system/e2e-desktop.service
COPY xorg-screen.conf /etc/systemd/system/e2e-desktop.service.d/screen.conf
RUN echo DISPLAY=:99 > /home/tester/.config/environment.d/display.conf
STOPSIGNAL SIGRTMIN+3
CMD ["/sbin/init"]
