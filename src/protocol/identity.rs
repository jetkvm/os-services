//! The KVM's identity. The KVM signs each session.ready with its Ed25519
//! key; the signature covers the sender's hello, so it proves the KVM holds
//! the key in this session. The user approves a KVM by the fingerprint of
//! that key.

use super::codec::invalid;
use ed25519_dalek::{Signature, VerifyingKey};
use sha2::{Digest, Sha256};
use std::{fmt, io};

/// The start of the signed bytes, so a signature made for session.ready
/// cannot serve any other purpose.
const SIGNATURE_CONTEXT: &[u8] = b"IP-KVM OS Services session.ready v5";
/// session.ready: version (4), token (16), public key (32), signature (64).
pub const READY_PAYLOAD_LEN: usize = 116;
const SIGNED_READY_LEN: usize = 52;
const FINGERPRINT_LEN: usize = 8;

/// The first 8 bytes of the SHA-256 digest of a KVM's public key: 64 bits,
/// too many to find another key with the same ID.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Fingerprint([u8; FINGERPRINT_LEN]);

impl Fingerprint {
    pub fn of(public_key: &[u8; 32]) -> Self {
        let digest = Sha256::digest(public_key);
        Self(digest[..FINGERPRINT_LEN].try_into().expect("8 bytes"))
    }

    /// Reads a fingerprint as the KVM shows it. Case, colons, dashes and spaces
    /// do not matter.
    pub fn parse(text: &str) -> Result<Self, String> {
        let digits: Vec<u8> = text
            .bytes()
            .filter(|byte| !matches!(byte, b'-' | b':' | b' '))
            .collect();
        let error = || format!("not a KVM fingerprint: {text}");
        if digits.len() != FINGERPRINT_LEN * 2 {
            return Err(error());
        }
        let mut bytes = [0; FINGERPRINT_LEN];
        for (byte, pair) in bytes.iter_mut().zip(digits.chunks(2)) {
            let pair = std::str::from_utf8(pair).map_err(|_| error())?;
            *byte = u8::from_str_radix(pair, 16).map_err(|_| error())?;
        }
        Ok(Self(bytes))
    }
}

/// Eight uppercase hex pairs joined by colons, like a MAC address.
impl fmt::Display for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, byte) in self.0.iter().enumerate() {
            if index > 0 {
                f.write_str(":")?;
            }
            write!(f, "{byte:02X}")?;
        }
        Ok(())
    }
}

/// Checks the signature in a session.ready payload against the hello
/// payload it answers; returns the fingerprint of the key that signed it.
pub fn verify_ready(hello: &[u8], ready: &[u8]) -> io::Result<Fingerprint> {
    if ready.len() != READY_PAYLOAD_LEN {
        return Err(invalid("invalid session.ready payload length"));
    }
    let (signed, signature) = ready.split_at(SIGNED_READY_LEN);
    let public_key: [u8; 32] = signed[20..].try_into().expect("32 bytes");
    let signature = Signature::from_bytes(signature.try_into().expect("64 bytes"));
    let key = VerifyingKey::from_bytes(&public_key)
        .map_err(|_| invalid("the KVM sent an invalid public key"))?;
    let message = [SIGNATURE_CONTEXT, hello, signed].concat();
    key.verify_strict(&message, &signature)
        .map_err(|_| invalid("the KVM's session.ready signature is invalid"))?;
    Ok(Fingerprint::of(&public_key))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A session.hello and session.ready pair that the KVM firmware
    /// produces for the key with seed [1; 32]; guest_identity_test.go in
    /// the firmware checks the same bytes.
    const HELLO: &str = "05000000\
        07070707070707070707070707070707\
        0909090909090909090909090909090909090909090909090909090909090909";
    const READY: &str = "05000000\
        07070707070707070707070707070707\
        8a88e3dd7409f195fd52db2d3cba5d72ca6709bf1d94121bf3748801b40f6f5c\
        d0026ddb131d87d728505167b1da2caec267d5db0c62aab8416bcfefbd86234d\
        6df84ce30155302f43ae2fbec53e6e2255fafd7ff834bd0056d46ba882672d08";
    const FINGERPRINT: &str = "34:75:0F:98:BD:59:FC:FC";

    fn hex(text: &str) -> Vec<u8> {
        (0..text.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn accepts_the_firmware_signature() {
        let fingerprint = verify_ready(&hex(HELLO), &hex(READY)).unwrap();
        assert_eq!(fingerprint.to_string(), FINGERPRINT);
    }

    #[test]
    fn rejects_a_signature_over_another_nonce() {
        let mut hello = hex(HELLO);
        hello[51] ^= 1;
        assert!(verify_ready(&hello, &hex(READY)).is_err());
    }

    #[test]
    fn rejects_a_signature_from_an_earlier_session() {
        // A rogue device that recorded this session.ready cannot replay it:
        // the next session has a new token and a new nonce.
        let mut hello = hex(HELLO);
        let mut ready = hex(READY);
        hello[4] ^= 1;
        ready[4] ^= 1;
        assert!(verify_ready(&hello, &ready).is_err());
    }

    #[test]
    fn rejects_a_signature_from_another_key() {
        let mut ready = hex(READY);
        ready[20] ^= 1;
        assert!(verify_ready(&hex(HELLO), &ready).is_err());
    }

    #[test]
    fn rejects_a_truncated_ready() {
        let ready = hex(READY);
        assert!(verify_ready(&hex(HELLO), &ready[..READY_PAYLOAD_LEN - 1]).is_err());
    }

    #[test]
    fn parses_fingerprints_as_people_type_them() {
        let expected = Fingerprint::parse(FINGERPRINT).unwrap();
        for text in [
            "34750f98bd59fcfc",
            "34-75-0f-98-bd-59-fc-fc",
            "3475 0F98 BD59 FCFC",
        ] {
            assert_eq!(Fingerprint::parse(text).unwrap(), expected, "{text}");
        }
        assert!(Fingerprint::parse("34:75:0F:98").is_err());
        assert!(Fingerprint::parse("X4:75:0F:98:BD:59:FC:FC").is_err());
        assert!(Fingerprint::parse("é5:0F:98:BD:59:FC:FC").is_err());
    }
}
