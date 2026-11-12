# Troubleshooting

To find the cause of a problem:

1. Read the log of the service. See [Logs](#logs).
2. Search this page for the error, and open it to see the fix.

## Logs

The service writes a line to its log when a session opens or closes, when video starts or stops, and when an error occurs. Every 5 seconds during video, it also writes a line with frame counts and timings.

| OS | Where to read the log |
|---|---|
| Linux | Run `journalctl --user -u ipkvm-os-services -f` |
| Linux without systemd | `~/.local/state/ipkvm-os-services/service.log` |
| macOS | `~/Library/Logs/IP-KVM OS Services/service.log` |
| Windows | `%LOCALAPPDATA%\IP-KVM OS Services\logs\service.log` |

Some fixes run a command. In these commands, `ipkvm-os-services` is the full path of the program on the computer. For the path on each operating system, see [Where the program is](install.md#where-the-program-is).

## The service does not connect to the KVM

<details>
<summary><code>no OS Services USB interface found</code></summary>

**Cause:** The KVM does not offer OS Services on this USB connection.

**Fix:** Make sure that the KVM's firmware supports OS Services, and that the KVM's USB cable connects to this computer.

</details>

<details>
<summary><code>the KVM … is not approved</code></summary>

**Cause:** You did not allow this KVM on this computer.

**Fix:** Compare the fingerprint in the log with the fingerprint in the KVM's OS Services settings. If they are the same, click **Allow** (**Yes** on Windows) in the dialog, or run the `--approve-kvm` command that the log shows.

</details>

<details>
<summary><code>the approved list … is damaged</code></summary>

**Cause:** The file that holds the KVMs you allowed cannot be read as a list of fingerprints. The service opens no session until the list is valid.

**Fix:** Run `ipkvm-os-services --approve-kvm <fingerprint>` with the fingerprint in the KVM's OS Services settings. The command replaces the damaged list with a list that holds only this KVM. Allow each other KVM again.

</details>

<details>
<summary><code>no dialog program found to ask for approval</code></summary>

**Cause:** Linux: `zenity` is not installed.

**Fix:** Install `zenity`, or run `ipkvm-os-services --approve-kvm <fingerprint>`.

</details>

<details>
<summary><code>permission denied opening the KVM</code></summary>

**Cause:** Linux: the USB access rule is missing or out of date.

**Fix:** Click **Allow USB Access** in the dialog, or run the command that the log shows. See [Linux installation](install.md#linux).

</details>

<details>
<summary><code>the KVM session is already in use</code></summary>

**Cause:** Another copy of the service uses the session.

**Fix:** Stop the other copy, or sign out and sign in again.

</details>

<details>
<summary><code>the KVM could not cancel its USB requests</code></summary>

**Cause:** The USB function of the KVM does not respond.

**Fix:** Restart the KVM.

</details>

## The KVM shows its HDMI capture

When the service sends no video, the KVM shows its HDMI capture. The clipboard, the terminal and file transfer continue. After a capture or encoding failure, the service tries video again after 10 seconds.

<details>
<summary><code>the KVM has video off; waiting for it to turn video on</code></summary>

**Cause:** Video is off in the KVM's settings, or the KVM stopped after too many video failures.

**Fix:** Turn on **Video** in the KVM's OS Services settings. If the KVM's web UI says that video switched too often, turn **Video** off and on again.

</details>

<details>
<summary><code>video paused: display … is inactive; waiting for wake</code></summary>

**Cause:** The display is asleep.

**Fix:** Wake the display. Video starts again when the display wakes.

</details>

<details>
<summary><code>video not available: this build has no Screen Recording grant</code></summary>

**Cause:** macOS did not give the Screen Recording permission.

**Fix:** Give the permission. See [macOS installation](install.md#macos). If the app is already in the list, remove it and add it again.

</details>

<details>
<summary><code>video not available: no hardware H.264 encoder is available</code></summary>

**Cause:** Windows or macOS: no hardware encoder opens.

**Fix:** Update the graphics driver.

</details>

### Linux

<details>
<summary><code>video not available: the installed capture helper is older than this service</code></summary>

**Cause:** The KVM updated the service, and the capture helper in `/usr/local/libexec` is older than the helper of the new version. Only an administrator can replace the helper. The service does not capture with an older helper, so the KVM shows its HDMI capture. The other services work.

**Fix:** In the dialog **Update the screen capture helper**, click **Update Helper** and enter an administrator password. Video starts without a new sign-in. If the dialog is closed, open **IP-KVM OS Services** from the applications menu, or sign out and sign in again: the dialog opens at each start of the service. Without a dialog program, run `sudo ~/.local/share/ipkvm-os-services/app/AppRun --install-linux-access`.

</details>

<details>
<summary><code>video not available: drmtap-helper is not installed, or this account may not run it</code></summary>

**Cause:** The capture helper is missing, or you are not a member of the `ipkvm-capture` group.

**Fix:** Run `sudo ~/.local/share/ipkvm-os-services/app/AppRun --install-linux-access`. This command installs the helper and adds you to the group. Then sign out and sign in again.

</details>

<details>
<summary><code>video not available: this account may not open the GPU's render node; add it to the render group</code></summary>

**Cause:** You are not a member of the `render` group. Then only the user of the active desktop session can open `/dev/dri/renderD*`. A screen lock that changes to a login screen removes this access. Another session on the same seat also removes it.

**Fix:** Run `sudo usermod -aG render <user>`, then sign out and sign in again. Video starts again automatically when the access returns.

</details>

<details>
<summary><code>video not available: no display is being scanned out</code></summary>

**Cause:** The display is off or asleep. Or the desktop is an X11 session on the NVIDIA proprietary driver, which does not use KMS.

**Fix:** Wake the display. Video starts again automatically. On NVIDIA graphics, the service sends video only in a Wayland session. See [NVIDIA](install.md#nvidia).

</details>

<details>
<summary><code>the graphics driver on this computer has no H.264 encoder, and OpenH264 is not installed</code></summary>

**Cause:** The VAAPI driver has no H.264 encoder, and the system has no OpenH264 library.

**Fix:** Install OpenH264: on Fedora, the `openh264` package, and on other distributions, `libopenh264`. See [Video encoders on Linux](install.md#video-encoders-on-linux).

</details>

<details>
<summary><code>video not available: the display's scanout format cannot be imported by the GPU's encoder</code> or <code>the GPU's hardware encoder would not open</code></summary>

**Cause:** The GPU or its driver cannot encode the display.

**Fix:** Use Intel or AMD graphics with a current Mesa VAAPI driver, or NVIDIA graphics with the proprietary driver.

</details>

<details>
<summary><code>video not available: libdrmtap is not installed, or is not the version this service was built against</code></summary>

**Cause:** The build does not contain the correct version of the library.

**Fix:** Use the AppImage. For a development build, run `packaging/libdrmtap/build.sh`.

</details>

## Display and resolution

<details>
<summary><code>multiple KVM displays</code></summary>

**Cause:** More than one display has the EDID of the KVM.

**Fix:** Select the capture display in the KVM's web UI.

</details>

<details>
<summary><code>KVM display not found</code></summary>

**Cause:** The KVM display did not appear in 10 seconds. If you did not select a display in the KVM's web UI, the service looks only for a JetKVM display.

**Fix:** Make sure that the KVM display is on in the display settings of the computer. If the KVM is not a JetKVM, select the capture display in the KVM's web UI. The log line shows the displays that the service found.

</details>

<details>
<summary><code>requested display layout not applied</code></summary>

**Cause:** The desktop did not apply the mirror or extend layout. On Linux, the service can set the layout only in an X11 session.

**Fix:** Arrange the displays in the display settings of the computer.

</details>

<details>
<summary><code>requested display mode not set</code></summary>

**Cause:** macOS or Windows did not apply the resolution from the KVM's settings.

**Fix:** Select the resolution in the display settings of the computer.

</details>

<details>
<summary>The resolution is different from the KVM's setting</summary>

On macOS and Windows, the service sets the KVM display to the resolution of the KVM each time the KVM applies its settings. The KVM applies its settings when video starts, when you turn on video, and when you change the resolution or the layout. A resolution that you select in the display settings of the computer stays until then.

To get the resolution of the KVM again, do one of these steps in the KVM's web UI:

- Turn video off and on again.
- Select a different resolution, then select the first resolution again.

On Linux, the service does not set the resolution. Each time that you turn on video or change the resolution or the layout, the KVM connects its display again, and the desktop uses the resolution of the KVM.

</details>

## Install and update

<details>
<summary><code>move IP-KVM OS Services to the Applications folder, then open it from there</code></summary>

**Cause:** macOS: you opened the app from the DMG or from a different folder.

**Fix:** Drag the app to **Applications**, then open it from there.

</details>

<details>
<summary><code>this installation cannot update itself from the KVM</code></summary>

**Cause:** The program does not come from an OS Services installer, for example a development build. On macOS, the cause can also be a service that you started manually.

**Fix:** No action is necessary. To get updates from the KVM, use an installer from a [release](https://github.com/jetkvm/os-services/releases/latest), and let the operating system start the service.

</details>
