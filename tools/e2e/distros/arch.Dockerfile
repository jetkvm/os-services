# Arch Linux, with no desktop: an X server on :99 on the host's display.
FROM archlinux:latest
COPY base.sh host-devices.sh /usr/local/sbin/
RUN sh /usr/local/sbin/base.sh
RUN pacman -Sy --noconfirm --needed xorg-server xf86-input-libinput xorg-xsetroot && pacman -Scc --noconfirm
COPY xorg-desktop.service /etc/systemd/system/e2e-desktop.service
COPY xorg-screen.conf /etc/systemd/system/e2e-desktop.service.d/screen.conf
RUN echo DISPLAY=:99 > /home/tester/.config/environment.d/display.conf
STOPSIGNAL SIGRTMIN+3
CMD ["/sbin/init"]
