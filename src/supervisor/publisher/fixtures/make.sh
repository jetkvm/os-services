#!/usr/bin/env bash
# Regenerates the fixtures of the publisher tests (../mod.rs) with GnuPG,
# throwaway keys and scripts/sign-appimage, the script that signs a release.
# Each fixture is a small ELF file with the two signature sections of an
# AppImage and a payload behind it. Needs gcc, objcopy, gpg and gpgsplit.
#
#   unsigned                 both sections empty
#   own                      signed by signing key 1 of the test release key
#   signing-key-2            signed by signing key 2
#   new-signing-key          signed by signing key 3, which `own` does not hold
#   release-key-second-in-block
#                            signed by signing key 1; the key block holds
#                            another publisher's key before the release key
#   other-publisher          signed by another publisher, with that key block
#   other-publisher-with-release-key-block
#                            signed by the other publisher; the key block is
#                            the release key's
#   other-publisher-with-both-key-blocks
#                            the same, with both keys in the block
#   other-publisher-grafted-signing-key
#                            the same; the block is the release key with the
#                            other publisher's signing key and binding added
#   signed-after-expiry      signed by signing key 1; the key block states
#                            that signing key 1 expired before the signature
#   revoked-signing-key      `own` with a key block in which the release key
#                            revoked signing key 1
#   revoked-release-key      `signing-key-2` with a key block in which the
#                            release key revoked itself
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
sign_appimage="$here/../../../../scripts/sign-appimage"
export GNUPGHOME="$(mktemp -d)"
work="$(mktemp -d)"
trap 'gpgconf --kill all; rm -rf "$GNUPGHOME" "$work"' EXIT
gpg_batch=(gpg --batch --quiet --pinentry-mode loopback --passphrase "")
fingerprints() { gpg --list-keys --with-colons "$1" | awk -F: '/^fpr:/ { print $10 }'; }

# The unsigned image: an object file with the sections at the sizes the
# AppImage runtime reserves, and a payload where an AppImage has its files.
head -c 1024 /dev/zero > "$work/signature"
head -c 8192 /dev/zero > "$work/key"
printf '' | gcc -x c -c - -o "$work/base.o"
objcopy --add-section .sha256_sig="$work/signature" --add-section .sig_key="$work/key" \
  "$work/base.o" "$here/unsigned.elf"
printf 'the files of the service\n' >> "$here/unsigned.elf"

# Puts a key block into the image's key section.
set_key_block() {
  local image="$1" block="$2" offset
  offset="$(readelf --section-headers --wide "$image" | sed 's/^ *\[ *[0-9]*\] *//' \
    | awk '$1 == ".sig_key" { print $4 }')"
  dd if=/dev/zero of="$image" bs=1 seek="$((16#$offset))" count=8192 conv=notrunc status=none
  dd if="$block" of="$image" bs=1 seek="$((16#$offset))" conv=notrunc status=none
}
# sign NAME SIGNING_KEY PRIMARY_KEY [KEY_BLOCK]: a signed copy of the
# unsigned image, with another key block when one is given.
sign() {
  cp "$here/unsigned.elf" "$here/$1.elf"
  "$sign_appimage" "$here/$1.elf" "$2!" "$3" 2>/dev/null
  [[ -z "${4:-}" ]] || set_key_block "$here/$1.elf" "$4"
}

"${gpg_batch[@]}" --quick-gen-key "Test release key" ed25519 cert never
release="$(fingerprints "Test release key" | sed -n 1p)"
"${gpg_batch[@]}" --quick-add-key "$release" ed25519 sign 1y
"${gpg_batch[@]}" --quick-add-key "$release" ed25519 sign 1y
key1="$(fingerprints "$release" | sed -n 2p)"
key2="$(fingerprints "$release" | sed -n 3p)"
"${gpg_batch[@]}" --quick-gen-key "Other publisher" ed25519 cert never
other="$(fingerprints "Other publisher" | sed -n 1p)"
"${gpg_batch[@]}" --quick-add-key "$other" ed25519 sign 1y
other_key="$(fingerprints "$other" | sed -n 2p)"

export_minimal=(gpg --export-options export-minimal --export)
"${export_minimal[@]}" --armor "$release" > "$work/release.asc"
"${export_minimal[@]}" --armor "$other" "$release" > "$work/other-then-release.asc"

sign own "$key1" "$release"
sign signing-key-2 "$key2" "$release"
sign release-key-second-in-block "$key1" "$release" "$work/other-then-release.asc"
sign other-publisher "$other_key" "$other"
sign other-publisher-with-release-key-block "$other_key" "$other" "$work/release.asc"
sign other-publisher-with-both-key-blocks "$other_key" "$other" "$work/other-then-release.asc"

# The other publisher's signing key and its binding, behind the packets of
# the release key.
"${export_minimal[@]}" "$release" > "$work/release.gpg"
"${export_minimal[@]}" "$other" > "$work/other.gpg"
mkdir "$work/split"
(cd "$work/split" && gpgsplit "$work/other.gpg")
cat "$work/release.gpg" "$work"/split/000004-014.public_subkey "$work"/split/000005-002.sig \
  | gpg --enarmor | sed 's/ARMORED FILE/PUBLIC KEY BLOCK/' > "$work/grafted.asc"
sign other-publisher-grafted-signing-key "$other_key" "$other" "$work/grafted.asc"

# Signing key 3, in a copy of the keyring: `own` holds two signing keys.
(
  cp -r "$GNUPGHOME" "$work/home3"
  export GNUPGHOME="$work/home3"
  trap 'gpgconf --kill all' EXIT
  "${gpg_batch[@]}" --quick-add-key "$release" ed25519 sign 1y
  sign new-signing-key "$(fingerprints "$release" | sed -n 4p)" "$release"
)

# A newer binding lets signing key 1 expire three seconds after the key was
# made; the signature comes after that. The times have a resolution of one
# second.
(
  cp -r "$GNUPGHOME" "$work/home-short"
  export GNUPGHOME="$work/home-short"
  trap 'gpgconf --kill all' EXIT
  sleep 2
  "${gpg_batch[@]}" --quick-set-expire "$release" seconds=3 "$key1"
  "${export_minimal[@]}" --armor "$release" > "$work/release-short.asc"
)
sleep 4
sign signed-after-expiry "$key1" "$release" "$work/release-short.asc"

# The release key revokes signing key 1: "key 1" selects it, revkey asks for
# a confirmation, a reason (0), a description (an empty line) and a second
# confirmation. Then the release key revokes itself, with the same answers.
printf 'key 1\nrevkey\ny\n0\n\ny\nsave\n' \
  | "${gpg_batch[@]}" --command-fd 0 --status-fd 2 --edit-key "$release" 2>/dev/null
gpg --list-keys --with-colons "$release" | grep -q '^sub:r:' || { echo "signing key 1 is not revoked" >&2; exit 1; }
"${export_minimal[@]}" --armor "$release" > "$work/release-revoked-key.asc"
cp "$here/own.elf" "$here/revoked-signing-key.elf"
set_key_block "$here/revoked-signing-key.elf" "$work/release-revoked-key.asc"
printf 'revkey\ny\n0\n\ny\nsave\n' \
  | "${gpg_batch[@]}" --command-fd 0 --status-fd 2 --edit-key "$release" 2>/dev/null
gpg --list-keys --with-colons "$release" | grep -q '^pub:r:' || { echo "the release key is not revoked" >&2; exit 1; }
"${export_minimal[@]}" --armor "$release" > "$work/release-revoked.asc"
cp "$here/signing-key-2.elf" "$here/revoked-release-key.elf"
set_key_block "$here/revoked-release-key.elf" "$work/release-revoked.asc"

ls -l "$here"/*.elf
