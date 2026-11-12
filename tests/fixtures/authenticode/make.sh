#!/bin/sh
# Makes the signed files that the tests of src/supervisor/authenticode.rs
# read. Run it in a Debian 13 container, from this folder:
#
#   docker run --rm -v "$PWD":/work -w /work debian:trixie sh make.sh
#
# The script makes its own certificate authorities, signs a small program
# with them, and deletes every private key when it ends. Every date is
# fixed, so the files give the same test results in any year: each signing
# certificate is valid for three days, as the certificates of Azure Artifact
# Signing are, and a timestamp gives the time of the signature.
set -eu
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq >/dev/null
apt-get install -y -qq gcc-mingw-w64-x86-64 osslsigncode openssl python3 >/dev/null 2>&1

out=$PWD
rm -f "$out"/*.exe
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
cd "$work"

# The program that is signed: it starts and ends.
printf 'int start(void) { return 0; }\n' > program.c
x86_64-w64-mingw32-gcc -Os -s -nostdlib -Wl,-e,start -Wl,--no-insert-timestamp \
  -o "$out/unsigned.exe" program.c

key() { openssl genrsa -out "$1.key" 2048 2>/dev/null; }

# root NAME SUBJECT
root() {
  key "$1"
  openssl req -x509 -new -key "$1.key" -subj "/CN=$2" -sha256 \
    -not_before 20200101000000Z -not_after 21200101000000Z \
    -addext "basicConstraints=critical,CA:true" \
    -addext "keyUsage=critical,keyCertSign,cRLSign" -out "$1.pem"
}

# certificate NAME SUBJECT ROOT FROM TO USAGE [KEY [SERIAL]]
# An empty USAGE makes a certificate without extended key usages.
certificate() {
  if [ -n "${7:-}" ]; then cp "$7.key" "$1.key"; else key "$1"; fi
  openssl req -new -key "$1.key" -subj "$2" -out "$1.csr"
  printf 'basicConstraints=critical,CA:false\nkeyUsage=critical,digitalSignature\n' > "$1.ext"
  if [ -n "$6" ]; then printf 'extendedKeyUsage=%s\n' "$6" >> "$1.ext"; fi
  if [ -n "${8:-}" ]; then serial="-set_serial $8"; else serial=-CAcreateserial; fi
  # shellcheck disable=SC2086
  openssl x509 -req -in "$1.csr" -CA "$3.pem" -CAkey "$3.key" $serial -sha256 \
    -not_before "$4" -not_after "$5" -extfile "$1.ext" -out "$1.pem" 2>/dev/null
  cat "$1.pem" "$3.pem" > "$1.chain.pem"
}

# Azure Artifact Signing names the certificate profile of the publisher in
# an extended key usage under 1.3.6.1.4.1.311.97. The usage 97.1.0 marks a
# public trust certificate and names nobody.
code=codeSigning,1.3.6.1.4.1.311.97.1.0
a=$code,1.3.6.1.4.1.311.97.990.1.1.1
b=$code,1.3.6.1.4.1.311.97.990.2.2.2
january="20260110000000Z 20260113000000Z"
june="20260610000000Z 20260613000000Z"
always="20200101000000Z 21200101000000Z"

root root "IP-KVM OS Services Test Root"
root other-root "IP-KVM OS Services Other Test Root"
# shellcheck disable=SC2086
{
  certificate service "/CN=Test Publisher A" root $january "$a"
  certificate same "/CN=Test Publisher A" root $june "$a" "" 0x5a5a
  certificate renamed "/CN=Test Publisher A Renamed" root $june "$a"
  certificate other-identity "/CN=Test Publisher A" root $june "$b"
  certificate under-other-root "/CN=Test Publisher A" other-root $june "$a"
  certificate subject-service "/O=Test Publisher S/CN=Test Publisher S" root $january codeSigning
  certificate subject-same "/O=Test Publisher S/CN=Test Publisher S" root $june codeSigning
  certificate subject-other "/O=Test Publisher T/CN=Test Publisher S" root $june codeSigning
  certificate no-usage-service "/CN=Test Publisher N" root $january ""
  certificate no-usage-same "/CN=Test Publisher N" root $june ""
  certificate time-server "/CN=Test Time Server" root $always critical,timeStamping
  certificate other-time-server "/CN=Other Test Time Server" other-root $always critical,timeStamping
  # Two certificates with one key, one name and one serial number. Only
  # the first one is for a time server.
  certificate twin "/CN=Test Twin" root $always critical,timeStamping "" 0x5151
  certificate twin-code "/CN=Test Twin" root $always critical,codeSigning twin 0x5151
  # A certificate with the name and the serial number of a certificate of
  # publisher A, and a key of its own.
  certificate forger "/CN=Test Publisher A" root $june "$a" "" 0x5a5a
  # A certificate with the identity of publisher A that the certificate of
  # publisher B issued, which is no certificate authority.
  certificate issued-by-b "/CN=Test Publisher A" other-identity $june "$a"
  cat issued-by-b.pem other-identity.chain.pem > issued-by-b.chain.pem
}

seconds() { date -u -d "$1" +%s; }
in_january=$(seconds '2026-01-11 12:00:00')
in_june=$(seconds '2026-06-11 12:00:00')
in_july=$(seconds '2026-07-01 12:00:00')

# sign FILE CERTIFICATES KEY [more osslsigncode arguments]
sign() {
  file=$1 certificates=$2 signer=$3
  shift 3
  osslsigncode sign -certs "$certificates" -key "$signer.key" -h sha256 \
    -in "$out/unsigned.exe" -out "$out/$file" "$@" >/dev/null
}
# nest SIGNED FILE SIGNER: adds a second signature, from June, to SIGNED.
nest() {
  # shellcheck disable=SC2046
  osslsigncode sign -nest -certs "$3.chain.pem" -key "$3.key" -h sha256 \
    -in "$out/$1" -out "$out/$2" $(stamp time-server "$in_june") >/dev/null
}
# stamp SERVER TIME: the arguments for a timestamp from that time server.
stamp() { echo "-TSA-certs $1.chain.pem -TSA-key $1.key -TSA-time $2"; }

# shellcheck disable=SC2046
{
  # The service: with the Azure identity, with only a subject, without a
  # timestamp, and with no root certificate in the file.
  sign service.exe service.chain.pem service $(stamp time-server "$in_january")
  sign service-subject.exe subject-service.chain.pem subject-service $(stamp time-server "$in_january")
  sign service-no-timestamp.exe service.chain.pem service
  sign service-no-usage.exe no-usage-service.chain.pem no-usage-service $(stamp time-server "$in_january")
  cp time-server.pem time-server-alone.chain.pem
  cp time-server.key time-server-alone.key
  sign service-no-root.exe service.pem service $(stamp time-server-alone "$in_january")

  # Installers.
  sign same-publisher.exe same.chain.pem same $(stamp time-server "$in_june")
  sign same-identity-other-name.exe renamed.chain.pem renamed $(stamp time-server "$in_june")
  sign other-identity-same-name.exe other-identity.chain.pem other-identity $(stamp time-server "$in_june")
  sign other-root.exe under-other-root.chain.pem under-other-root $(stamp other-time-server "$in_june")
  sign same-subject.exe subject-same.chain.pem subject-same $(stamp time-server "$in_june")
  sign other-subject.exe subject-other.chain.pem subject-other $(stamp time-server "$in_june")
  sign same-no-usage.exe no-usage-same.chain.pem no-usage-same $(stamp time-server "$in_june")

  # Installers of the publisher whose time of signature is not proven.
  sign no-timestamp.exe same.chain.pem same
  sign claimed-time.exe same.chain.pem same -time "$in_june"
  sign timestamp-after-expiry.exe same.chain.pem same $(stamp time-server "$in_july")
  sign timestamp-from-other-root.exe same.chain.pem same $(stamp other-time-server "$in_june")
  sign twin.exe same.chain.pem same $(stamp twin "$in_june")

  # Installers that only claim the publisher.
  sign forger.exe forger.chain.pem forger $(stamp time-server "$in_june")
  sign certificate-from-another-publisher.exe issued-by-b.chain.pem issued-by-b $(stamp time-server "$in_june")

  # Installers with two signatures: the second one is nested in the first.
  sign first-by-a.exe same.chain.pem same $(stamp time-server "$in_june")
  nest first-by-a.exe second-signature-by-another-publisher.exe other-identity
  sign first-by-b.exe other-identity.chain.pem other-identity $(stamp time-server "$in_june")
  nest first-by-b.exe second-signature-by-the-publisher.exe same
}

python3 - "$out" <<'PY'
import subprocess, sys
out = sys.argv[1]

def der(name):
    return subprocess.run(["openssl", "x509", "-in", name + ".pem", "-outform", "der"],
                          check=True, capture_output=True).stdout

# osslsigncode makes no timestamp with a certificate that is not for a time
# server, so the time server certificate in the file is replaced by its twin.
server, code = der("twin"), der("twin-code")
signed = open(out + "/twin.exe", "rb").read()
assert len(server) == len(code) and server in signed
open(out + "/timestamp-by-code-signer.exe", "wb").write(signed.replace(server, code))

# The signature of the forger's key, with the certificate of publisher A in
# the place of the forger's certificate.
forger, publisher = der("forger"), der("same")
signed = open(out + "/forger.exe", "rb").read()
assert len(forger) == len(publisher) and forger in signed
open(out + "/signature-by-another-key.exe", "wb").write(signed.replace(forger, publisher))

# The signature block of the service is there, and Windows cannot read it:
# the first byte of the signed message is wrong. The block is the fifth
# entry of the data directory of the program header (PE32+), after an
# eight-byte header of its own.
import struct
signed = bytearray(open(out + "/service.exe", "rb").read())
header = struct.unpack_from("<I", signed, 0x3c)[0]
assert signed[header:header + 4] == b"PE\0\0" and signed[header + 24:header + 26] == b"\x0b\x02"
block, size = struct.unpack_from("<II", signed, header + 24 + 112 + 4 * 8)
assert size > 8 and signed[block + 8] == 0x30
signed[block + 8] = 0x31
open(out + "/service-unreadable-signature.exe", "wb").write(signed)

# One byte of the program changes after the signature.
signed = bytearray(open(out + "/same-publisher.exe", "rb").read())
signed[0x50] ^= 0xff
open(out + "/changed-after-signing.exe", "wb").write(signed)
PY
rm "$out/twin.exe" "$out/forger.exe" "$out/first-by-a.exe" "$out/first-by-b.exe"
# The files get the owner of this script, whoever the container runs as.
chown "$(stat -c %u:%g "$out/make.sh")" "$out"/*.exe
ls -l "$out"
