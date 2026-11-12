#!/usr/bin/env bash
# Build libdrmtap and its privileged helper, which is how the Linux sender
# reaches the scanout without holding a capability itself.
#
# Two modes:
#   --stage DIR   build and drop the library, the helper and the license in DIR
#                 (no privileges). The packaged builds use this, and ship
#                 what it produces.
#   (no options)  build, install system-wide and make the helper privileged.
#                 For a development machine running a sender straight from
#                 cargo, which carries no bundled copy.
#
# The helper is the only privileged part, and it is installed 0750 to a group
# so that a file capability does not hand every local account a screen reader.
# It drops to CAP_SYS_ADMIN under seccomp before it serves a single grab.
set -euo pipefail

VERSION=0.5.4
REPO=https://github.com/fxd0h/libdrmtap
COMMIT=5da68a3a368db569716d0d0f11cefacbb11b2290
SONAME=libdrmtap.so.$VERSION
GROUP=ipkvm-capture
HELPER_PATH=/usr/local/libexec/drmtap-helper

STAGE=""
case "${1:-}" in
  --stage) STAGE=${2:?--stage needs a directory} ;;
  "") ;;
  *) echo "usage: $0 [--stage DIR]" >&2; exit 2 ;;
esac

HERE=$(cd "$(dirname "$0")" && pwd)
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

echo "building libdrmtap v$VERSION"
git clone -q "$REPO" "$WORK/src"
git -C "$WORK/src" checkout -q "$COMMIT"
# The patches in this folder change libdrmtap and have its license (MIT).
for patch in "$HERE"/*.patch; do
  [ -e "$patch" ] || continue
  echo "applying $(basename "$patch")"
  git -C "$WORK/src" apply "$patch"
done
# The C++ side compiles against a vendored copy of this header, so a commit
# whose structs moved would be read through the wrong offsets across a dlopen
# boundary. Fail loudly rather than at run time.
VENDORED=$(cd "$HERE/../../src/video/kms/native" && pwd)/drmtap.h
if ! cmp -s "$WORK/src/include/drmtap.h" "$VENDORED"; then
  echo "libdrmtap $COMMIT ships a different drmtap.h than $VENDORED" >&2
  echo "copy it across and rebuild the service before shipping this" >&2
  exit 1
fi

# libdrmtap's EGL is only for detiling into CPU pixels, which the sender
# never asks for: it imports the scanout dma-buf itself.
meson setup "$WORK/build" "$WORK/src" -Degl=disabled --buildtype=release >/dev/null
meson compile -C "$WORK/build" >/dev/null

if [ -n "$STAGE" ]; then
  mkdir -p "$STAGE"
  install -m 0755 "$WORK/build/$SONAME" "$STAGE/$SONAME"
  # Staged without its capability on purpose: a file capability cannot survive
  # inside an AppImage's read-only image, so the AppImage grants it on the
  # installed copy instead.
  install -m 0755 "$WORK/build/drmtap-helper" "$STAGE/drmtap-helper"
  # The notices of the AppImage read these two.
  install -m 0644 "$WORK/src/LICENSE" "$STAGE/LICENSE"
  echo "${REPO%.git}/tree/$COMMIT" > "$STAGE/source"
  echo "staged $SONAME, drmtap-helper and the license in $STAGE"
  exit 0
fi

echo "installing (sudo)"
sudo meson install -C "$WORK/build" --quiet
sudo groupadd -f "$GROUP"
sudo chown "root:$GROUP" "$HELPER_PATH"
sudo chmod 0750 "$HELPER_PATH"
sudo setcap cap_sys_admin+ep "$HELPER_PATH"
sudo ldconfig
echo "helper: $(getcap "$HELPER_PATH")"
echo "add each user that may capture: sudo usermod -aG $GROUP <user>, then log in again"
