# Vendored vcpkg manifest and ports

`vcpkg.json` lists the native libraries that the builds link: FFmpeg, and on Windows the Intel Media SDK dispatcher for QSV. `cargo xtask setup` installs them with vcpkg at the `builtin-baseline` of the manifest. The dispatcher comes from vcpkg's own port.

`ports/ffmpeg` builds FFmpeg 9.0.2. Its build steps in `portfile.cmake` come from the FFmpeg port of <https://github.com/microsoft/vcpkg> (vcpkg release 2026.10.16), and so do the patches with a number. The component list, `build.sh.in` and the patches named `ffmpeg-*` are this repository's.

A port of its own builds FFmpeg because vcpkg's FFmpeg port selects large groups of components, and the service needs only a few encoders.

## Licenses

The files in this folder are under three licenses:

- `ports/ffmpeg/portfile.cmake` and `ports/ffmpeg/vcpkg.json` start from vcpkg's FFmpeg port, which is under the MIT license in [LICENSE.txt](LICENSE.txt), copyright Microsoft Corporation. This repository's changes to them are under the MIT license in the repository's [LICENSE](../../LICENSE).
- `vcpkg.json` and `ports/ffmpeg/build.sh.in` are this repository's, under its MIT license.
- Every `.patch` file changes FFmpeg's source, so it is under FFmpeg's license, the GNU LGPL version 2.1 or later. This applies to the numbered patches from vcpkg and to the `ffmpeg-*` patches of this repository. `portfile.cmake` installs FFmpeg's `COPYING.LGPLv2.1` with the built libraries, and the installers' third-party notices include it.

## Changes from upstream

- `ports/ffmpeg/ffmpeg-amf-bitrate.patch`, `ffmpeg-videotoolbox-bitrate.patch` and `ffmpeg-nvenc-bitrate.patch` make the encoders apply a `bit_rate` that changed after init to the running encoder, from the next frame on, with no reset and no IDR. AMF compares the encoder's target bitrate property with `bit_rate` before each frame. VideoToolbox records the session's rate at the first frame and sets the session property when `bit_rate` differs. NVENC already reconfigures its rate control on a change, and the patch removes the encoder reset and the IDR that FFmpeg adds to it.
- `ports/ffmpeg/ffmpeg-windows-dll-search.patch` makes FFmpeg load DLLs, such as the AMF, NVENC and Quick Sync runtimes, only from System32 and not from the folder of the executable.
- `ports/ffmpeg/ffmpeg-videotoolbox-low-delay.patch` changes the FFmpeg VideoToolbox encoder in low-delay mode. The encoder returns each frame as soon as it is encoded. It does not keep the frame until the next input.
- `ports/ffmpeg/ffmpeg-vaapi-x2bgr10.patch` maps the 10-bit BGR format (DRM `XBGR2101010`, VAAPI `X2B10G10R10`) between DRM, VAAPI and FFmpeg, as FFmpeg maps the 10-bit RGB one. KWin scans out a 10-bit desktop in either order, and the KMS capture maps the scanout to a VAAPI surface through these tables.
- `ports/ffmpeg/portfile.cmake` builds only the hardware encoders that the service opens, and the hardware contexts they need. It builds no decoders, parsers, decode accelerators, bitstream filters or `avformat`, because the service only encodes. It does not pass `--enable-gpl`, because no enabled component needs it, and it fails when FFmpeg's configure reports a license other than LGPL 2.1. It passes `--disable-zlib`, because vcpkg's `0004-dependencies.patch` makes an enabled zlib a pkg-config requirement and no encoder uses zlib.
- On Linux, `ports/ffmpeg/portfile.cmake` builds `avfilter` with the `buffer`, `buffersink` and `hwmap` filters for the KMS capture, and does not enable AMF, which needs headers that Linux does not use.
- On Windows, `ports/ffmpeg/portfile.cmake` adds the dispatcher's `include/mfx` folder to the compiler's include path, because FFmpeg includes the dispatcher's headers without that folder.
- `ports/ffmpeg/portfile.cmake` and `build.sh.in` build static release libraries for the three triplets that `cargo xtask setup` uses, and install no pkg-config files, because the build scripts link the libraries by name. The parts of vcpkg's port for other targets, for shared and debug builds, and for CMake projects that look for FFmpeg are removed.
- FFmpeg links libva directly. The KMS capture creates its VAAPI device through `hwmap`, which calls `vaapi_device_derive`, so libva's functions must be available without `vaapi_device_create`.
