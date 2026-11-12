# Install IP-KVM OS Services

The service runs in the desktop session of the user who is signed in on the computer that the KVM is connected to. The operating system starts the service when that user signs in, and starts it again if it fails. You change all settings in the KVM's web UI. The service has no settings of its own. When the service sends no video, the KVM shows the video from its HDMI capture, and the clipboard, the terminal and file transfer still work.

## Get the installer

Each [release](https://github.com/jetkvm/os-services/releases/latest) has the installer for each operating system:

| OS | Installer |
|---|---|
| Windows | The `windows-x86_64-setup.exe` file |
| macOS | The `macos-aarch64.dmg` file |
| Linux | The `linux-x86_64.tar.gz` file, which contains the AppImage |

After you install the service, allow the KVM. See [Allow the KVM](#allow-the-kvm).

## Windows

Run the installer. It installs the service for the current user, in `%LOCALAPPDATA%\IP-KVM OS Services`, and starts it. The service then starts each time you sign in.

## macOS

1. Open the DMG and drag **IP-KVM OS Services** to **Applications**.
2. Open **IP-KVM OS Services** from **Applications**. The app starts the service and shows a confirmation. The service then starts each time you sign in.
3. Give the service the Screen Recording permission. macOS asks for it the first time that the service connects to the KVM. If you do not see the request, open **System Settings > Privacy & Security > Screen & System Audio Recording** and turn on **IP-KVM OS Services**.

Without the permission, the KVM shows its HDMI capture.

## Linux

1. Extract the `.tar.gz` file.
2. Open the AppImage from a file manager, or run `./<file>.AppImage`. The service copies its files to `~/.local/share/ipkvm-os-services` and starts from there. The applications menu gets the entry **IP-KVM OS Services**. You can then delete the AppImage.
3. At **Allow USB access?**, click **Allow USB Access** and enter an administrator password. The service installs the USB access rule and the capture helper, adds you to the `ipkvm-capture` group, and connects to the KVM.
4. Sign out and sign in again. Capture needs the new group membership.

To install the parts one at a time, for example for other users, run these commands:

```sh
sudo ~/.local/share/ipkvm-os-services/app/AppRun --install-linux-usb-access
sudo ~/.local/share/ipkvm-os-services/app/AppRun --install-linux-capture-access
sudo usermod -aG ipkvm-capture <user>
```

If the AppImage does not start or shows an error, check that the computer has what it needs:

- glibc 2.35 or newer, for example Debian 12, Ubuntu 22.04 or Fedora 36 and newer. On an older system, the AppImage does not start.
- The VAAPI, EGL and GLES libraries of the computer. These libraries must match the graphics drivers, so the AppImage does not include them. A desktop installation has them. On a minimal installation, install them first. For example, on Ubuntu:

  ```sh
  sudo apt install libva2 libva-drm2 libegl1 libgles2
  ```

- FUSE, only to open the AppImage. If the AppImage reports a FUSE error, run `./<file>.AppImage --appimage-extract-and-run`, or install the `fusermount3` program (the `fuse3` package on Debian and Ubuntu). The installed service does not use FUSE.
- `zenity` for the dialogs. Without `zenity`, use the commands in [Allow the KVM](#allow-the-kvm) and in step 3.

### Window managers and computers without systemd

The service starts at sign-in through the systemd user unit and the autostart entry of the desktop. Two kinds of computer need more:

- **A window manager without a desktop environment, such as sway or i3.** Some of them run no autostart entries. If the service does not start when you sign in, start it from the configuration of the window manager. For sway and i3, add this line to the configuration file:

  ```
  exec ~/.local/share/ipkvm-os-services/app/AppRun --install-service
  ```

- **A computer without systemd.** The install needs no extra step. The autostart entry starts the program, and the program keeps the service running: it starts the service again after a failure and after an update. The log is the file `~/.local/state/ipkvm-os-services/service.log`.

### Video encoders on Linux

On Linux, the GPU encodes the video with VAAPI (Intel and AMD) or NVENC (NVIDIA, Wayland only). If the computer has no H.264 encoder, for example on Fedora, the service shows a dialog that tells you what to install. Until then, the KVM shows its HDMI capture.

### NVIDIA

On the NVIDIA proprietary driver, the service sends video only in a Wayland session. In an X11 session, the KVM shows its HDMI capture.

## Allow the KVM

When a KVM connects for the first time, the service shows the **Allow this KVM?** dialog with the KVM's fingerprint. The KVM's web UI shows its fingerprint in the OS Services settings.

1. Compare the two fingerprints.
2. If they are the same, click **Allow** (**Yes** on Windows). If they are different, click **Don't Allow** (**No** on Windows).

Each user of the computer allows the KVM one time.

To allow or remove a KVM from a terminal, run one of these commands as the desktop user:

```sh
ipkvm-os-services --approve-kvm <fingerprint>
ipkvm-os-services --forget-kvm <fingerprint>
```

If you remove a KVM that is connected, the service ends its session in approximately one second. The **Allow this KVM?** dialog then shows again.

## Where the program is

Some steps in this document and in [Troubleshooting](troubleshooting.md) run the program from a terminal. No installer adds the program to the `PATH`. Use the full path:

| OS | Program |
|---|---|
| Windows | `"$env:LOCALAPPDATA\IP-KVM OS Services\ipkvm-os-services.exe"` (PowerShell) |
| macOS | `"/Applications/IP-KVM OS Services.app/Contents/MacOS/ipkvm-os-services"` |
| Linux | `~/.local/share/ipkvm-os-services/app/AppRun` |

## Update

A KVM that implements the update service of the [protocol](../PROTOCOL.md#update-service) keeps the service up to date. The KVM sends each release over USB, so the computer needs no network access. The KVM selects the release and verifies its signature. The service checks the size and the hash of the package, installs it and starts again. The KVM decides when to update, so see the KVM's documentation for its update settings.

On macOS, the service also checks the code signature of the new app. A service from a release installs only an app that the same Developer ID team signed. The check needs no network access. If the check fails, the service keeps the installed version and reports the failure to the KVM.

The update starts the service again, so the video from the service stops for a few seconds.

The KVM can update a service that you installed with the Windows installer, the macOS app or the AppImage. On Linux, the update replaces the files in `~/.local/share/ipkvm-os-services`. A service that still runs from an AppImage, as releases up to 0.4.5 do, moves to this folder when the KVM updates it; you can then delete the AppImage.

The update cannot replace the capture helper in `/usr/local/libexec`, because only an administrator can. If the new version carries a newer helper, the service shows the dialog **Update the screen capture helper**. Click **Update Helper** and enter an administrator password. Until then, the KVM shows its HDMI capture, and the other services work. See [Troubleshooting](troubleshooting.md) if the dialog is closed.

On Linux, a release also checks who made the package. It installs the package only if the package carries a signature from the release key of the installed release. If the check fails, the service keeps the installed version and tells the KVM the reason. For this reason, a release does not install a development release, which has no signature.

On Windows, a service that has an Authenticode signature runs only an installer that the publisher of the service signed. It deletes any other installer and tells the KVM why. The Windows release has no Authenticode signature, so it does not do this check.

If the service cannot replace itself, for example because you do not have write access to its folder, update it manually with the installer from a [release](https://github.com/jetkvm/os-services/releases/latest):

- **Windows:** Install the new release over the old one. The installer starts the service again.
- **macOS:** Replace the app in **Applications**. Then open the app to start the service again.
- **Linux:** Open the AppImage of the new release. It replaces the installed files and starts the service again.

## Remove

- **Windows:**
  1. Remove **IP-KVM OS Services** in **Settings > Apps**. The uninstaller removes the service. It keeps the log and the list of allowed KVMs.
  2. Optional: Delete the log and the list of allowed KVMs. In PowerShell:

     ```powershell
     Remove-Item -Recurse "$env:LOCALAPPDATA\IP-KVM OS Services"
     Remove-Item -Recurse "$env:LOCALAPPDATA\ipkvm-os-services"
     ```

- **macOS:**
  1. Remove the registration:

     ```sh
     "/Applications/IP-KVM OS Services.app/Contents/MacOS/ipkvm-os-services" --uninstall-service
     ```

  2. Delete the app from **Applications**.
  3. Open **System Settings > Privacy & Security > Screen & System Audio Recording**. Select **IP-KVM OS Services**, and then click the remove button (**−**).
  4. Optional: Delete the list of allowed KVMs and the log:

     ```sh
     rm -r ~/Library/Application\ Support/ipkvm-os-services
     rm -r ~/Library/Logs/IP-KVM\ OS\ Services
     ```

  If you deleted the app before step 1, the registration stays, and macOS tries to start the service each time you sign in. Remove the registration by hand:

  ```sh
  launchctl bootout gui/$(id -u)/com.jetkvm.os-services
  rm ~/Library/LaunchAgents/com.jetkvm.os-services.plist
  ```

- **Linux:**
  1. Remove the registration:

     ```sh
     ~/.local/share/ipkvm-os-services/app/AppRun --uninstall-service
     ```

  2. Delete the files of the service:

     ```sh
     rm -r ~/.local/share/ipkvm-os-services
     ```

  3. Optional: Delete the list of allowed KVMs. On a computer without systemd, also delete the log:

     ```sh
     rm -r ~/.config/ipkvm-os-services
     rm -r ~/.local/state/ipkvm-os-services
     ```

  4. All users of the computer share the USB access rule, the capture helper and the `ipkvm-capture` group. If no other user uses the service, delete them and reload the udev rules:

     ```sh
     sudo rm /etc/udev/rules.d/70-ipkvm-os-services.rules
     sudo rm /usr/local/libexec/drmtap-helper
     sudo groupdel ipkvm-capture
     sudo udevadm control --reload
     ```

  If you deleted the files before step 1, the registration stays, and the desktop tries to start the service each time you sign in. Remove the registration by hand:

  ```sh
  systemctl --user disable --now ipkvm-os-services.service
  rm ~/.config/systemd/user/ipkvm-os-services.service
  rm ~/.config/autostart/ipkvm-os-services.desktop
  rm ~/.local/share/applications/ipkvm-os-services.desktop
  systemctl --user daemon-reload
  ```

  On a computer without systemd, run only the two `rm` commands for the `.desktop` files.
