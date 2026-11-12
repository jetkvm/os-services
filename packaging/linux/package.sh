#!/usr/bin/env bash
# Builds the AppImage from the release build and the staged libdrmtap.
# `cargo xtask package` runs it in two steps and writes the notices into the
# AppDir between them:
#   package.sh deploy    copies the program, libdrmtap and the libraries they
#                        need into target/release/appdir
#   package.sh appimage  packs target/release/appdir into an AppImage in
#                        target/release/bundle/appimage
set -euo pipefail
umask 022
cd "$(dirname "$0")/../.."
version=$(sed -n 's/^version = "\([^"]*\)"/\1/p' Cargo.toml | head -1)
output="$PWD/target/release/bundle"
appdir="$PWD/target/release/appdir"
bundled="$appdir/usr/lib/ipkvm-os-services"
cache="${XDG_CACHE_HOME:-$HOME/.cache}/ipkvm-os-services-packaging"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

case "${1:-}" in
  deploy)
    rm -rf "$appdir"
    mkdir -p "$bundled"
    install -m755 packaging/linux/drmtap/drmtap-helper "$bundled/"
    library=$(find packaging/linux/drmtap -name "libdrmtap.so.0.*" | head -1)
    install -m644 "$library" "$bundled/libdrmtap.so.0"
    mkdir -p "$cache"
    # Each tool is a fixed release with a fixed SHA-256, and a cached copy
    # is checked too.
    for tool in \
      "linuxdeploy 1-alpha-20251107-1 c20cd71e3a4e3b80c3483cef793cda3f4e990aca14014d23c544ca3ce1270b4d" \
      "linuxdeploy-plugin-appimage 1-alpha-20250213-1 992d502a248e14ab185448ddf6f6e7d25558cb84d4623c354c3af350c25fccb3"; do
      read -r name release sha256 <<< "$tool"
      file="$cache/$name-x86_64.AppImage"
      if ! echo "$sha256  $file" | sha256sum --check --status 2>/dev/null; then
        curl -fL --retry 3 "https://github.com/linuxdeploy/$name/releases/download/$release/$name-x86_64.AppImage" -o "$file"
        echo "$sha256  $file" | sha256sum --check --quiet
      fi
      chmod +x "$file"
    done
    ;;
  appimage)
    mkdir -p "$output/appimage"
    ;;
  *)
    echo "usage: $0 deploy|appimage" >&2
    exit 2
    ;;
esac

cat > "$work/ipkvm-os-services.desktop" <<'EOF'
[Desktop Entry]
Type=Application
Name=IP-KVM OS Services
Exec=ipkvm-os-services
Icon=ipkvm-os-services
Categories=Utility;
Terminal=false
EOF
export APPIMAGE_EXTRACT_AND_RUN=1
export PATH="$cache:$PATH"
export OUTPUT="$output/appimage/IP-KVM-OS-Services_${version}_amd64.AppImage"
arguments=(
  --appdir "$appdir" --executable target/release/ipkvm-os-services
  --deploy-deps-only "$bundled"
  --desktop-file "$work/ipkvm-os-services.desktop"
  --icon-file packaging/icons/icon.png --icon-filename ipkvm-os-services
  # The host supplies VAAPI and EGL/GLES alongside its GPU driver.
  --exclude-library 'libva*' --exclude-library 'libEGL*'
  --exclude-library 'libGLES*'
)
if [[ "$1" == appimage ]]; then
  arguments+=(--output appimage)
fi
"$cache/linuxdeploy-x86_64.AppImage" --appimage-extract-and-run "${arguments[@]}"

# The AppImage runs on glibc 2.35 and newer (docs/install.md). A bundled
# library that needs a newer one would raise that minimum, so the build
# fails instead. Weak symbols do not count.
minimum=2.35
newest=$({ find "$appdir/usr" -type f \( -name '*.so*' -o -perm -u+x \) -exec objdump -T {} + 2>/dev/null || true; } |
    awk '$2 != "w" && match($0, /GLIBC_2\.[0-9]+/) { print substr($0, RSTART + 6, RLENGTH - 6) }' |
    sort -V | tail -1)
if [[ "$(printf '%s\n%s\n' "$newest" "$minimum" | sort -V | tail -1)" != "$minimum" ]]; then
    echo "the AppImage needs glibc $newest, more than the minimum $minimum in docs/install.md" >&2
    exit 1
fi
