# Develop IP-KVM OS Services

This document tells you how to build and test the service.

## Repository

| Path | Contents |
|---|---|
| `src/` | The service |
| `xtask/` | The build, test and packaging commands (`cargo xtask`) |
| `tools/` | The test tools. See [Test tools](#test-tools). |
| `packaging/` | Installer resources, service files and the build of the Linux capture helper |
| `vendor/` | The vcpkg manifest and the ports that build FFmpeg, with local changes |

## Set up the build

1. Install the tools for your operating system:

   | OS | Tools |
   |---|---|
   | All | [rustup](https://rustup.rs), Git, and [Go 1.24](https://go.dev/dl/) for the test tools |
   | Windows | Visual Studio 2022 Build Tools with the C++ workload |
   | macOS | Xcode Command Line Tools, and `brew install pkg-config nasm` |
   | Linux | Docker, or the packages in the `toolchain` stage of the [Dockerfile](../Dockerfile) |

   rustup installs the Rust version that [rust-toolchain.toml](../rust-toolchain.toml) specifies.

2. Run `cargo xtask setup` one time.

`cargo xtask setup` builds FFmpeg with vcpkg. On Linux, it also builds libdrmtap.

## Build commands

| Command | What it does |
|---|---|
| `cargo xtask build` | Builds the service in release mode. |
| `cargo xtask test` | Runs the unit tests of the service and the Go tools. |
| `cargo xtask lint` | Checks formatting, clippy, gofmt and go vet. |
| `cargo xtask package` | Builds the installers for this operating system into `dist/`. |
| `cargo xtask ci` | Runs setup, lint, test and package, as the CI job does. |
| `cargo xtask docker` | Runs `cargo xtask ci` for Linux in Docker and copies `dist/` out. |
| `cargo xtask e2e -- <lab file>` | Builds the checkout and tests it on the computers of a lab. See [tools/e2e](../tools/e2e/README.md). |
| `cargo xtask perf -- <lab file>` | Builds the checkout and measures its video pipeline on the computers of a lab. See [tools/perf](../tools/perf/README.md). |

These options change the commands:

- `--skip-go` removes the Go tools from `test`, `lint` and `ci`.
- `--hardware` adds the tests that need a hardware encoder to `test` and `ci`.

## Run a build

If two copies of the service run, they take the USB session of the KVM from each other. Stop the installed service before you run a build.

1. Stop the installed service:

   | OS | Command |
   |---|---|
   | Linux | `systemctl --user stop ipkvm-os-services` |
   | Linux without systemd | `pkill -xf "$HOME/.local/share/ipkvm-os-services/app/AppRun --run"` |
   | macOS | `launchctl bootout gui/$(id -u)/com.jetkvm.os-services` |
   | Windows (PowerShell) | `& "$env:LOCALAPPDATA\IP-KVM OS Services\ipkvm-os-services.exe" --stop` |

2. Run `target/release/ipkvm-os-services --run`. On Linux and macOS, the service writes its log to the terminal. On Windows, it writes its log to the log file.
3. To stop the build on Linux or macOS, press Ctrl+C. On Windows, run the command in step 1 again.
4. To start the installed service again, open the app, or sign out and sign in again.

On macOS, a service that you start manually does not offer updates to the KVM.

## Test video capture without a KVM

This test is available only on Linux. `--capture-test` runs the same capture and hardware encoding as a KVM session. It writes the video to a file and does not send it to a KVM. The result shows if a problem is in capture and encoding on this computer, or in the connection to the KVM.

```sh
./<file>.AppImage --capture-test out.h264 30
```

This command records 30 seconds of the display to `out.h264`. Then it shows the frame rate and the bitrate that it got. To play the file, use `ffplay out.h264` or VLC.

- To change the bitrate during the test, as a KVM does when its network changes, add `--capture-test-retarget-after SECONDS KBPS`. After that number of seconds, the encoder gets the new bitrate in kbit/s.
- To select the display, add `--capture-test-display NAME`.
- To encode H.265 instead of H.264, add `--capture-test-h265`.

On a computer with NVIDIA's driver, the test also writes the time of each stage of a frame every 5 seconds, in a line that starts with `kms nvenc stages`. The stages are `pace` (the wait for the next frame), `grab`, `cursor`, `draw`, `copy` and `encode`. Each stage shows its median and its 95th percentile in milliseconds. To get these lines from a service that you run with `--run`, set the environment variable `OS_SERVICES_KMS_STAGES=1`.

## Test tools

| Tool | Use it to |
|---|---|
| [e2e](../tools/e2e/README.md) | Test a build end to end on real computers through a real KVM. |
| [perf](../tools/perf/README.md) | Measure the frame rate, the stage times and the key-to-video latency of a build, with the same lab file as e2e. |
