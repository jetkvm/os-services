//! Tells the user at the computer that there is no H.264 encoder: the
//! graphics driver has none, and OpenH264 is not installed. The probes find
//! it every 2 s, so the dialog opens once per start. While video is off, the KVM shows the computer's HDMI output, so
//! the dialog also reaches a user at the KVM.

use crate::{approval::Dialog, usb_access::zenity};
use std::{
    process::Command,
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::Duration,
};

const TITLE: &str = "No video encoder";
/// The PCI vendor ID of AMD, whose VAAPI driver is Mesa's.
const AMD: &str = "0x1002";

/// Shows the dialog, the first time only, and writes the text to the log.
pub fn tell_once() {
    static SHOWN: AtomicBool = AtomicBool::new(false);
    if SHOWN.swap(true, Ordering::Relaxed) {
        return;
    }
    let text = text(fedora(), amd_gpu());
    eprintln!("ipkvm-os-services: {}", text.replace("\n\n", " "));
    thread::spawn(move || {
        let mut notify = Command::new("notify-send");
        notify.args(["--urgency", "critical", TITLE, &text]);
        let mut dialog = Dialog::spawn(vec![zenity("--warning", TITLE, &text, &[]), notify]);
        // The dialog stays until the user closes it.
        while dialog.is_open() {
            let _ = dialog.approved();
            thread::sleep(Duration::from_millis(250));
        }
    });
}

/// Fedora removes H.264 from its Mesa, ships OpenH264 from Cisco's
/// repository, and RPM Fusion has the full AMD driver.
fn text(fedora: bool, amd_gpu: bool) -> String {
    let mut text = String::from(
        "IP-KVM OS Services has no video: the graphics driver on this computer has no \
         H.264 encoder, and OpenH264 is not installed.",
    );
    if fedora {
        text.push_str("\n\nOn Fedora, install the openh264 package.");
        if amd_gpu {
            text.push_str(
                " For encoding on the graphics card instead, install \
                 mesa-va-drivers-freeworld from RPM Fusion.",
            );
        }
    } else {
        text.push_str("\n\nInstall OpenH264 (libopenh264) to encode in software.");
    }
    text
}

fn fedora() -> bool {
    std::fs::read_to_string("/etc/os-release")
        .is_ok_and(|release| release.lines().any(|line| line == "ID=fedora"))
}

fn amd_gpu() -> bool {
    let Ok(cards) = std::fs::read_dir("/sys/class/drm") else {
        return false;
    };
    cards.flatten().any(|card| {
        std::fs::read_to_string(card.path().join("device/vendor"))
            .is_ok_and(|vendor| vendor.trim() == AMD)
    })
}
