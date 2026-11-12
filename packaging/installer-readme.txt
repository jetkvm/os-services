IP-KVM OS Services @VERSION@

This disc has an installer for Windows, macOS and Linux.

INSTALL

Windows
  Run the .exe file.

macOS
  1. Open the .dmg file.
  2. Drag IP-KVM OS Services to Applications.
  3. Open IP-KVM OS Services from Applications.

Linux
  The AppImage is in a .tar.gz archive, which keeps it executable.
  1. Copy the .tar.gz file from the disc to your home folder.
  2. Extract it: in the file manager, select Extract Here, or run
     "tar -xzf <file>.tar.gz".
  3. Open the extracted .AppImage file.
  The service copies its files to ~/.local/share/ipkvm-os-services, so you
  can then delete the .AppImage file.

PERMISSIONS

All
  The service asks "Allow this KVM?" and shows a fingerprint. Compare it
  with the fingerprint in the KVM's web UI, in the OS Services settings.
  Click Allow (Yes on Windows) only when the two are the same.

macOS
  Give the Screen Recording permission when macOS asks. If you miss the
  prompt, open System Settings > Privacy & Security > Screen & System
  Audio Recording, and turn on IP-KVM OS Services.

Linux
  The service asks "Allow USB access?". Click Allow USB Access and enter
  an administrator password. Then sign out and sign in again, so that
  capture works.
  The dialogs need zenity. If a dialog does not open, use a terminal:
     ~/.local/share/ipkvm-os-services/app/AppRun --approve-kvm <fingerprint>
     sudo ~/.local/share/ipkvm-os-services/app/AppRun --install-linux-access
  Then sign out and sign in again.

REMOVE

Windows
  Open Settings > Apps, select IP-KVM OS Services, then select Uninstall.

macOS
  1. In Terminal, run:
     "/Applications/IP-KVM OS Services.app/Contents/MacOS/ipkvm-os-services" --uninstall-service
  2. Move IP-KVM OS Services from Applications to the Trash.

Linux
  In a terminal, run:
     ~/.local/share/ipkvm-os-services/app/AppRun --uninstall-service
     rm -r ~/.local/share/ipkvm-os-services
