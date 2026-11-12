# Security

## Report a vulnerability

Report a vulnerability privately through GitHub:

1. Go to the **Security** tab of the repository.
2. Select **Report a vulnerability**.
3. Include the version, the operating system and the steps that show the problem.

Do not open a public issue for a vulnerability.

## What the service can do

A report is in scope if it lets someone use these abilities without the permission of the KVM:

- **Screen capture.** The service reads the screen of the desktop session that it runs in. On Linux, the `drmtap-helper` program has the `CAP_SYS_ADMIN` capability to read the scanout. Only members of the `ipkvm-capture` group can run it.
- **Shell access.** The KVM can open a shell as the user who is signed in. The settings of the KVM turn this ability on and off.
- **File writes.** The KVM can write files into the Downloads folder of the user, or into the home folder if there is no Downloads folder. The settings of the KVM turn this ability on and off. On Windows and macOS, the service marks each file as a download, so the OS checks the file before it opens it.
- **Clipboard.** The KVM can read and set the clipboard text while clipboard sharing is on.
- **Updates.** The KVM can replace the installed service with a package that it sends. The service checks the size and the SHA-256 hash that the KVM sends with the package. The KVM must verify the package signature before it sends the package.
  - On macOS, a service that a Developer ID team signed also checks the code signature of the app in the package. It installs the app only if the signature is valid and comes from the same team. A service without such a signature, such as a development build, does not check.
  - On Windows, a service that has an Authenticode signature also checks the installer. It runs only an installer that the publisher of the service signed. The Windows release has no Authenticode signature, so it does not do this check.
  - On Linux, a release AppImage carries a signature of the release key and the public part of that key. An installed release accepts a package only if the package carries a signature from the same release key. An AppImage without a signature, such as a development build, accepts every package.

The service accepts these requests only through the USB connection of the KVM, and only from a KVM that the signed-in user allowed. The service opens no network port. All users with access to the web UI of the KVM can use these abilities, so the access control of the KVM protects them.

## Out of scope

- **A device between the computer and the KVM on the USB cable.** The keyboard of the KVM uses the same cable without protection. Such a device can read and type keystrokes, including the login password.
- **A KVM that an attacker controls.** The user allowed the key of that KVM, so the service trusts it by design. This includes the update ability where the service does not check the publisher of a package: such a KVM can install any program as the service.
