# Vendored nusb

This folder is nusb 0.2.7 with one change. `Cargo.toml` of the repository replaces the crate from crates.io with this folder (`[patch.crates-io]`).

The source is the crate file <https://static.crates.io/crates/nusb/nusb-0.2.7.crate>, SHA-256 `18ef13beb3b3a8fc16fd7aea912ebd3d45dde00a9a5b968d0742297468065845`. It is commit `bdc148c123c102785cd1d506b77bfeeb794ffeb1` of <https://github.com/kevinmehall/nusb>. The folder holds every file of the crate except `.cargo-ok`, `.cargo/`, `.github/` and `.gitignore`. nusb is under the Apache-2.0 or the MIT license, in `LICENSE-APACHE` and `LICENSE-MIT`.

## Change from upstream

[reap-after-disconnect.patch](reap-after-disconnect.patch) is the change, as a diff against the crate file. To make this folder again from the crate file, extract the crate file and apply the patch with `patch -p1`.

On Linux, nusb stopped reaping a device at the first `USBDEVFS_REAPURBNDELAY` that the kernel answered with `ENODEV`. The kernel answers `ENODEV` when the device is marked as not attached and no completed URB waits to be reaped. `usb_disconnect` sets that mark before it completes the URBs of the device, so URBs can complete after the first `ENODEV`. nusb never reaped those URBs. Their transfers stayed pending for good, and they kept their buffers and the device file open. The service lost one device file each time the KVM left the bus.

With the patch, the first `ENODEV` changes the epoll watch on the device file to edge-triggered and keeps it. The kernel wakes the file for each URB that completes, and one more time when it removes the device, after it has completed every URB. At each wake-up the event thread reaps every URB that has completed. An unexpected error from the reap does not stop that work, because no later wake-up is sure to come: the event thread tries again until no URB is left, or until the eighth error at one wake-up. The watch is edge-triggered because the file of a disconnected device is always ready (`EPOLLHUP`): a level-triggered watch makes the event thread spin.

nusb 0.2.9, the latest release, has the same fault. Remove this folder and the `[patch.crates-io]` entry when a release of nusb reaps after a disconnect.
