# IP-KVM OS Services

> [!NOTE]
> IP-KVM OS Services is experimental. Expect bugs and breaking changes. Report problems in [issues](https://github.com/jetkvm/os-services/issues).

IP-KVM OS Services is a small service for the computer that a KVM is connected to. It gives the KVM functions that the KVM hardware cannot supply:

- Video up to 3840x2160
- A shared clipboard
- A terminal
- File transfer to the computer

The service and the [IP-KVM OS Services protocol](PROTOCOL.md) are vendor-neutral, so any KVM can implement the protocol.

## Supported KVMs

| KVM | Firmware | OS Services |
|---|---|---|
| JetKVM Mini | [jetkvm/mini](https://github.com/jetkvm/mini) | Supported |
| JetKVM | [jetkvm/kvm](https://github.com/jetkvm/kvm) | Not supported |

## Requirements

- A [supported KVM](#supported-kvms).
- One of these computers:

  | OS | Architecture | Video encoder |
  |---|---|---|
  | Windows 11 | x64 | Intel Quick Sync, NVIDIA NVENC or AMD AMF |
  | macOS 12.3 or later | Apple silicon | VideoToolbox |
  | Linux with KMS | x64 | VAAPI (Intel or AMD graphics), or NVENC (NVIDIA proprietary driver, Wayland only). If the VAAPI driver has no H.264 encoder, the CPU encodes with OpenH264. |

## Install

Download the installer for your operating system from the [latest release](https://github.com/jetkvm/os-services/releases/latest), on the computer that the KVM is connected to. When the service asks **Allow this KVM?**, allow it only if the fingerprint is the same as the fingerprint in the KVM's OS Services settings.

### Windows

1. Run `windows-x86_64-setup.exe`.
2. At **Allow this KVM?**, compare the fingerprints and click **Yes**.

To remove: **Settings > Apps > IP-KVM OS Services > Uninstall**.

### macOS

1. Open `macos-aarch64.dmg` and drag **IP-KVM OS Services** to **Applications**.
2. Open **IP-KVM OS Services**.
3. At **Allow this KVM?**, compare the fingerprints and click **Allow**.
4. Give the Screen Recording permission. If you miss the request, turn it on in **System Settings > Privacy & Security > Screen & System Audio Recording**.

To remove, run this command, then move the app to the Trash:

```sh
"/Applications/IP-KVM OS Services.app/Contents/MacOS/ipkvm-os-services" --uninstall-service
```

### Linux

1. Extract `linux-x86_64.tar.gz` and open the AppImage. The service installs itself in `~/.local/share/ipkvm-os-services`.
2. At **Allow this KVM?**, compare the fingerprints and click **Allow**.
3. At **Allow USB access?**, click **Allow USB Access** and enter an administrator password.
4. Sign out and sign in again, so that screen capture works.

To remove, run these commands:

```sh
~/.local/share/ipkvm-os-services/app/AppRun --uninstall-service
rm -r ~/.local/share/ipkvm-os-services
```

For more detail, see [Install IP-KVM OS Services](docs/install.md) and [Troubleshooting](docs/troubleshooting.md).

## Documentation

### Use the service

| Document | Contents |
|---|---|
| [Install](docs/install.md) | Per-OS install, update and removal |
| [Troubleshooting](docs/troubleshooting.md) | Logs and common problems |
| [Security](SECURITY.md) | Vulnerability reports |

### Develop or implement

| Document | Contents |
|---|---|
| [Development](docs/development.md) | Build and test |
| [Protocol](PROTOCOL.md) | The service–KVM protocol |

## License

The code is licensed under [MIT](LICENSE).
