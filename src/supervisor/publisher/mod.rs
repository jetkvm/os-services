//! The publisher of an AppImage. A release AppImage carries the release
//! signature in two sections of its ELF header, in the form that
//! appimagetool defines: `.sha256_sig` holds an armored OpenPGP signature
//! over the SHA-256 of the image, written in hex and computed with both
//! sections as zeros, and `.sig_key` holds the armored key block of the key
//! that signed.
//!
//! A package comes from the publisher of an installation when its key block
//! is the certificate of the installation's primary key, and a signing key
//! of that primary key made its signature. The primary key vouches for its
//! signing keys inside the key block, so a release may sign with a signing
//! key that the installation has not seen. Sequoia applies the OpenPGP rules
//! for that: bindings, key flags, expiry and revocation. The rules apply at
//! the time the signature states, so the check does not depend on the clock
//! of the computer.
//!
//! The signature does not cover the key block, so whoever sends a package
//! can change the block. An installation therefore does not take the key
//! block of a package as it is. It keeps one certificate: its own and the
//! package's together, with only what the primary key itself signed. A
//! revocation that an installed release stated stays known, and nothing
//! that another party added stays.

use object::{Object, ObjectSection, ReadCache};
use sequoia_openpgp::{
    self as openpgp,
    parse::{
        stream::{DetachedVerifierBuilder, MessageLayer, MessageStructure, VerificationHelper},
        Parse,
    },
    policy::StandardPolicy,
    serialize::SerializeInto,
    Cert, KeyHandle, Packet, PacketPile,
};
use sha2::{Digest, Sha256};
use std::{
    fs::{File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    path::Path,
    time::SystemTime,
};

const KEY_SECTION: &str = ".sig_key";
const SIGNATURE_SECTION: &str = ".sha256_sig";

/// The AppImage runtime reserves 8192 bytes for the key block and 1024 for
/// the signature. A package that states a larger section is refused before
/// the section is read.
const MAX_SECTION: u64 = 1 << 16;

/// A section's place in the image file.
#[derive(Clone, Copy)]
struct Place {
    offset: u64,
    size: u64,
}

/// The key block and the signature sections of an open image.
struct Sections {
    file: File,
    key: Place,
    signature: Place,
}

/// The key block of the image, or None when the image has none: a file that
/// is not an AppImage with the two sections, or an AppImage that was not
/// signed.
pub fn key_block(image: &Path) -> Result<Option<Vec<u8>>, String> {
    let file = File::open(image)
        .map_err(|error| format!("could not read {}: {error}", image.display()))?;
    let Ok(mut sections) = sections(file) else {
        return Ok(None);
    };
    let block = sections
        .read(sections.key)
        .map_err(|error| format!("could not read {}: {error}", image.display()))?;
    Ok((!block.is_empty()).then_some(block))
}

/// Writes the key block into the image's key section, with zeros behind it.
pub fn set_key_block(image: &Path, key_block: &[u8]) -> Result<(), String> {
    let failed = |error: io::Error| format!("could not write {}: {error}", image.display());
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(image)
        .map_err(failed)?;
    let mut sections = sections(file)?;
    let mut bytes = vec![0; sections.key.size as usize];
    bytes
        .get_mut(..key_block.len())
        .ok_or("the key block is larger than the key section")?
        .copy_from_slice(key_block);
    sections
        .file
        .seek(SeekFrom::Start(sections.key.offset))
        .and_then(|_| sections.file.write_all(&bytes))
        .and_then(|()| sections.file.sync_all())
        .map_err(failed)
}

/// Checks a package for an installed image with the key block `own`, or
/// with none when no release signed the image. With a key block, the
/// package must carry a signature from its publisher. The key section of
/// the package must have room for the key block that the installation keeps
/// (see `adopt`), because the install writes it there before the package
/// replaces the image.
pub fn check(own: Option<&[u8]>, package: &Path) -> Result<(), String> {
    match adopt(own, package)? {
        Some(kept) => room(package, &kept),
        None => Ok(()),
    }
}

/// Fails unless the key section of the image has room for the key block.
fn room(image: &Path, key_block: &[u8]) -> Result<(), String> {
    let file = File::open(image)
        .map_err(|error| format!("could not read {}: {error}", image.display()))?;
    if key_block.len() as u64 > sections(file)?.key.size {
        return Err("the package is refused: the publisher's key does not fit in it".into());
    }
    Ok(())
}

/// The key block that an installation keeps when it installs the package.
/// An installation with the key block `own` keeps the certificate of its
/// publisher, with what the package's key block adds. An installation
/// without a key block takes the certificate of a package whose signature
/// is good, and it stays without one for any other package.
pub fn adopt(own: Option<&[u8]>, package: &Path) -> Result<Option<Vec<u8>>, String> {
    let certificate = match (own, publisher(own, package)) {
        (_, Ok(certificate)) => certificate,
        (Some(_), Err(refusal)) => return Err(refusal),
        (None, Err(_)) => return Ok(None),
    };
    kept(&certificate)
        .map(Some)
        .map_err(|error| format!("the publisher's key cannot be kept: {error}"))
}

/// The certificate that signed the package: the one of `own` when the
/// installation has a key block, else the one that the package states.
fn publisher(own: Option<&[u8]>, package: &Path) -> Result<Cert, String> {
    // A KVM can show as little as the first 64 characters of the message,
    // so the reason comes early.
    verify(own, package).map_err(|reason| format!("the package is refused: {reason}"))
}

fn verify(own: Option<&[u8]>, package: &Path) -> Result<Cert, String> {
    let own = own
        .map(Cert::from_bytes)
        .transpose()
        .map_err(|error| format!("the key block of this installation is damaged: {error}"))?;
    let file = File::open(package).map_err(|error| format!("it cannot be read: {error}"))?;
    let mut sections = sections(file)?;
    let read = |error: io::Error| format!("it cannot be read: {error}");
    let key_block = sections.read(sections.key).map_err(read)?;
    let signature = sections.read(sections.signature).map_err(read)?;
    if key_block.is_empty() || signature.is_empty() {
        return Err("it is not signed".into());
    }
    let digest = sections.digest().map_err(read)?;

    // One certificate, as a release writes it.
    let stated = Cert::from_bytes(&key_block).map_err(|error| format!("its key block: {error}"))?;
    let publisher = match own {
        Some(own) if own.fingerprint() != stated.fingerprint() => {
            return Err("another publisher signed it".into());
        }
        // What both key blocks state about the key: a revocation in either
        // one counts, and so does a signing key that only the package's
        // block has.
        Some(own) => own
            .merge_public(stated)
            .map_err(|error| format!("its key block: {error}"))?,
        None => stated,
    };

    let time = signature_time(&signature)?;
    let policy = StandardPolicy::new();
    DetachedVerifierBuilder::from_bytes(&signature)
        .and_then(|builder| builder.with_policy(&policy, time, Publisher(publisher.clone())))
        .and_then(|mut verifier| verifier.verify_bytes(digest.as_bytes()))
        .map_err(|error| error.to_string())?;
    Ok(publisher)
}

/// The certificate as an installation keeps it: the primary key, its user
/// IDs and its subkeys, each with the signatures and revocations that the
/// primary key made. Sequoia sorts a signature under these only after it
/// has checked the primary key's signature on it, so a part that the
/// primary key did not sign is left out.
fn kept(certificate: &Cert) -> openpgp::Result<Vec<u8>> {
    let primary = certificate.primary_key();
    let mut packets = vec![Packet::from(primary.key().clone())];
    packets.extend(primary.self_signatures().cloned().map(Packet::from));
    packets.extend(primary.self_revocations().cloned().map(Packet::from));
    for user in certificate.userids() {
        let signed: Vec<Packet> = user
            .self_signatures()
            .chain(user.self_revocations())
            .cloned()
            .map(Packet::from)
            .collect();
        if !signed.is_empty() {
            packets.push(user.userid().clone().into());
            packets.extend(signed);
        }
    }
    for subkey in certificate.keys().subkeys() {
        let signed: Vec<Packet> = subkey
            .self_signatures()
            .chain(subkey.self_revocations())
            .cloned()
            .map(Packet::from)
            .collect();
        if !signed.is_empty() {
            packets.push(subkey.key().clone().into());
            packets.extend(signed);
        }
    }
    Cert::from_packets(packets.into_iter())?.armored().to_vec()
}

/// The time the one signature states for its creation.
fn signature_time(signature: &[u8]) -> Result<SystemTime, String> {
    let packets =
        PacketPile::from_bytes(signature).map_err(|error| format!("its signature: {error}"))?;
    let mut signatures = packets.descendants().filter_map(|packet| match packet {
        Packet::Signature(signature) => Some(signature),
        _ => None,
    });
    let (Some(signature), None) = (signatures.next(), signatures.next()) else {
        return Err("it does not hold exactly one signature".into());
    };
    signature
        .signature_creation_time()
        .ok_or_else(|| "its signature states no time".into())
}

/// Gives the verifier the publisher's certificate, and accepts the package
/// when one of that certificate's keys made a good signature.
struct Publisher(Cert);

impl VerificationHelper for Publisher {
    fn get_certs(&mut self, _: &[KeyHandle]) -> openpgp::Result<Vec<Cert>> {
        Ok(vec![self.0.clone()])
    }

    fn check(&mut self, structure: MessageStructure) -> openpgp::Result<()> {
        let mut failure = "it holds no signature".to_owned();
        for layer in structure {
            if let MessageLayer::SignatureGroup { results } = layer {
                for result in results {
                    match result {
                        Ok(_) => return Ok(()),
                        Err(error) => failure = error.to_string(),
                    }
                }
            }
        }
        Err(openpgp::anyhow::anyhow!(failure))
    }
}

fn sections(file: File) -> Result<Sections, String> {
    let cache = ReadCache::new(file);
    let (key, signature) = {
        let elf = object::File::parse(&cache).map_err(|_| "it is not an AppImage")?;
        let place = |name: &str| {
            let (offset, size) = elf
                .section_by_name(name)
                .and_then(|section| section.file_range())
                .ok_or(format!("it has no {name} section"))?;
            if size > MAX_SECTION {
                return Err(format!("its {name} section is too large"));
            }
            Ok(Place { offset, size })
        };
        (place(KEY_SECTION)?, place(SIGNATURE_SECTION)?)
    };
    Ok(Sections {
        file: cache.into_inner(),
        key,
        signature,
    })
}

impl Sections {
    /// The section's bytes up to the zeros that fill the rest of it.
    fn read(&mut self, place: Place) -> io::Result<Vec<u8>> {
        let mut bytes = vec![0; place.size as usize];
        self.file.seek(SeekFrom::Start(place.offset))?;
        self.file.read_exact(&mut bytes)?;
        let end = bytes
            .iter()
            .position(|&byte| byte == 0)
            .unwrap_or(bytes.len());
        bytes.truncate(end);
        Ok(bytes)
    }

    /// SHA-256 of the image with both sections as zeros, in hex: the text
    /// that the signature signs.
    fn digest(&mut self) -> io::Result<String> {
        self.file.seek(SeekFrom::Start(0))?;
        let mut hasher = Sha256::new();
        let mut buffer = vec![0; 1 << 16];
        let mut position = 0u64;
        loop {
            let count = self.file.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            let chunk = &mut buffer[..count];
            for place in [self.key, self.signature] {
                let start = place.offset.max(position);
                let end = (place.offset.saturating_add(place.size)).min(position + count as u64);
                if start < end {
                    chunk[(start - position) as usize..(end - position) as usize].fill(0);
                }
            }
            hasher.update(&*chunk);
            position += count as u64;
        }
        Ok(hasher
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        path::PathBuf,
        sync::atomic::{AtomicU32, Ordering},
    };

    /// Small ELF files with the two sections of an AppImage, signed by
    /// `scripts/sign-appimage` with throwaway keys (see fixtures/make.sh).
    /// `own` is signed by signing key 1 of the test release key.
    macro_rules! fixture {
        ($name:literal) => {
            (
                $name,
                include_bytes!(concat!("fixtures/", $name, ".elf")).as_slice(),
            )
        };
    }

    const OWN: (&str, &[u8]) = fixture!("own");

    /// Writes the fixture to a file of its own and returns the path. The
    /// tests run at the same time, and several use the same fixture.
    fn image((name, bytes): (&str, &[u8])) -> PathBuf {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let directory = std::env::temp_dir().join(format!("publisher-{}", std::process::id()));
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join(format!("{}-{name}", NEXT.fetch_add(1, Ordering::Relaxed)));
        fs::write(&path, bytes).unwrap();
        path
    }

    /// The key block of a signed fixture.
    fn key_block_of(fixture: (&str, &[u8])) -> Vec<u8> {
        key_block(&image(fixture)).unwrap().expect("a signed image")
    }

    /// The check of a package by an installation that runs the image `own`.
    fn checked(own: (&str, &[u8]), package: (&str, &[u8])) -> Result<(), String> {
        check(Some(&key_block_of(own)), &image(package))
    }

    /// A copy of the fixture with another key block.
    fn with_key_block(fixture: (&str, &[u8]), key_block: &[u8]) -> PathBuf {
        let package = image(fixture);
        set_key_block(&package, key_block).unwrap();
        package
    }

    #[test]
    fn a_package_signed_by_another_signing_key_of_the_release_key_is_accepted() {
        checked(OWN, fixture!("signing-key-2")).unwrap();
    }

    #[test]
    fn a_package_signed_by_a_signing_key_the_installation_has_not_seen_is_accepted() {
        checked(OWN, fixture!("new-signing-key")).unwrap();
    }

    #[test]
    fn a_key_block_with_two_keys_is_refused() {
        let two = checked(OWN, fixture!("release-key-second-in-block"));
        assert!(two.unwrap_err().contains("its key block"));
    }

    #[test]
    fn an_installation_keeps_a_revocation_that_a_later_package_does_not_state() {
        // The installed release states that signing key 1 is revoked. The
        // package is signed by signing key 2, and its key block does not
        // state the revocation.
        let own = key_block_of(fixture!("revoked-signing-key"));
        let package = image(fixture!("signing-key-2"));
        let kept = adopt(Some(&own), &package).unwrap().unwrap();
        // `own` is signed by signing key 1.
        check(Some(&kept), &image(OWN)).unwrap_err();
        check(Some(&kept), &package).unwrap();
    }

    #[test]
    fn an_installation_keeps_a_signing_key_that_an_installed_package_stated() {
        let own = key_block_of(OWN);
        // Signed by signing key 3, with a key block that does not hold it.
        let package = with_key_block(fixture!("new-signing-key"), &own);
        check(Some(&own), &package).unwrap_err();
        let kept = adopt(Some(&own), &image(fixture!("new-signing-key")))
            .unwrap()
            .unwrap();
        check(Some(&kept), &package).unwrap();
    }

    #[test]
    fn the_kept_key_block_holds_only_what_the_release_key_signed() {
        // Signed by signing key 2. The key block also holds a signing key of
        // another publisher, with that publisher's binding.
        let grafted = key_block_of(fixture!("other-publisher-grafted-signing-key"));
        let package = with_key_block(fixture!("signing-key-2"), &grafted);
        let own = key_block_of(OWN);
        check(Some(&own), &package).unwrap();
        let kept = adopt(Some(&own), &package).unwrap().unwrap();
        let subkeys = |block: &[u8]| Cert::from_bytes(block).unwrap().keys().subkeys().count();
        assert_eq!((subkeys(&grafted), subkeys(&kept)), (3, 2));
    }

    #[test]
    fn an_installation_without_a_key_block_takes_the_publisher_of_a_good_package() {
        let kept = adopt(None, &image(fixture!("signing-key-2")))
            .unwrap()
            .unwrap();
        check(Some(&kept), &image(OWN)).unwrap();
        check(Some(&kept), &image(fixture!("other-publisher"))).unwrap_err();
        // An unsigned package, and a package whose signature is not good.
        assert_eq!(adopt(None, &image(fixture!("unsigned"))), Ok(None));
        let (name, bytes) = fixture!("signing-key-2");
        let mut changed = bytes.to_vec();
        *changed.last_mut().unwrap() ^= 0xff;
        let changed = image((&format!("{name}-changed"), &changed));
        assert_eq!(adopt(None, &changed), Ok(None));
    }

    #[test]
    fn an_installation_does_not_keep_the_key_of_a_package_that_it_refuses() {
        let own = key_block_of(OWN);
        adopt(Some(&own), &image(fixture!("other-publisher"))).unwrap_err();
    }

    #[test]
    fn a_package_with_a_changed_byte_is_refused() {
        let (name, bytes) = fixture!("signing-key-2");
        let mut changed = bytes.to_vec();
        // The last byte is in the payload behind the ELF file.
        *changed.last_mut().unwrap() ^= 0xff;
        let refusal = checked(OWN, (&format!("{name}-changed"), &changed)).unwrap_err();
        assert!(refusal.contains("manipulated"), "{refusal}");
    }

    #[test]
    fn an_unsigned_package_is_refused() {
        let unsigned = checked(OWN, fixture!("unsigned"));
        assert!(unsigned.unwrap_err().contains("not signed"));
    }

    #[test]
    fn a_file_that_is_not_an_appimage_is_refused() {
        let other = checked(OWN, ("text", b"not an image"));
        assert!(other.unwrap_err().contains("not an AppImage"));
    }

    #[test]
    fn a_package_of_another_publisher_is_refused() {
        let other = checked(OWN, fixture!("other-publisher"));
        assert!(other.unwrap_err().contains("another publisher"));
    }

    #[test]
    fn another_publisher_cannot_borrow_the_release_key_block() {
        for package in [
            fixture!("other-publisher-with-release-key-block"),
            fixture!("other-publisher-with-both-key-blocks"),
            fixture!("other-publisher-grafted-signing-key"),
        ] {
            checked(OWN, package).unwrap_err();
        }
    }

    #[test]
    fn a_signature_made_after_the_signing_key_expired_is_refused() {
        checked(OWN, fixture!("signed-after-expiry")).unwrap_err();
    }

    #[test]
    fn a_revoked_signing_key_is_refused_from_either_key_block() {
        // The package's key block states the revocation.
        checked(OWN, fixture!("revoked-signing-key")).unwrap_err();
        // Only the installation's key block states it.
        checked(fixture!("revoked-signing-key"), OWN).unwrap_err();
        // The other signing key of the same release key is still good.
        checked(fixture!("revoked-signing-key"), fixture!("signing-key-2")).unwrap();
    }

    #[test]
    fn a_revoked_release_key_is_refused() {
        checked(OWN, fixture!("revoked-release-key")).unwrap_err();
    }

    #[test]
    fn an_image_without_a_key_block_has_no_publisher() {
        assert_eq!(key_block(&image(fixture!("unsigned"))), Ok(None));
        assert_eq!(key_block(&image(("text", b"not an image"))), Ok(None));
    }

    #[test]
    fn the_key_block_that_is_kept_must_fit_the_key_section_of_the_package() {
        // The fixtures have the key section of an AppImage: 8192 bytes.
        let package = image(OWN);
        room(&package, &[0; 8192]).unwrap();
        let refusal = room(&package, &[0; 8193]).unwrap_err();
        assert!(refusal.contains("does not fit"), "{refusal}");
    }

    #[test]
    fn an_unsigned_installation_checks_the_room_for_the_key_block_it_takes() {
        // Without a key block of its own, the installation takes the one
        // of a package whose signature is good, and accepts any other.
        check(None, &image(OWN)).unwrap();
        check(None, &image(fixture!("unsigned"))).unwrap();
    }
}
