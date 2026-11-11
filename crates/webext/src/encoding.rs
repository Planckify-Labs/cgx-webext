//! Hashing and encoding helpers shared by package and manifest handling.

use base64::Engine;
use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
use sha2::{Digest, Sha256};

pub fn sha256(data: &[u8]) -> [u8; 32] {
    Sha256::digest(data).into()
}

/// Lenient base64: accepts standard and URL-safe alphabets, optional padding and embedded whitespace.
pub fn base64_decode(text: &str) -> Option<Vec<u8>> {
    const ENGINE: GeneralPurpose = GeneralPurpose::new(
        &base64::alphabet::STANDARD,
        GeneralPurposeConfig::new().with_decode_allow_trailing_bits(true).with_decode_padding_mode(DecodePaddingMode::Indifferent),
    );
    let cleaned: String = text
        .chars()
        .filter(|c| !c.is_whitespace())
        .map(|c| match c {
            '-' => '+',
            '_' => '/',
            other => other,
        })
        .collect();
    ENGINE.decode(cleaned).ok()
}

/// Standard base64 with padding.
pub fn base64_encode(data: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(data)
}

/// Chrome extension IDs write 16 bytes as 32 letters in the alphabet `a`..=`p`.
pub fn extension_id_from_bytes(bytes: &[u8]) -> String {
    bytes.iter().take(16).flat_map(|b| [(b'a' + (b >> 4)) as char, (b'a' + (b & 0xF)) as char]).collect()
}

/// The Chrome extension ID derived from a DER SubjectPublicKeyInfo.
pub fn chrome_extension_id_from_public_key(public_key_der: &[u8]) -> String {
    extension_id_from_bytes(&sha256(public_key_der)[..16])
}

pub fn is_valid_chrome_extension_id(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|b| (b'a'..=b'p').contains(&b))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn sha256_known_vector() {
        assert_eq!(hex(&sha256(b"abc")), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    }

    #[test]
    fn base64_variants() {
        assert_eq!(base64_decode("aGVsbG8=").unwrap(), b"hello");
        assert_eq!(base64_decode("aGVsbG8").unwrap(), b"hello");
        assert_eq!(base64_decode("aGVs\nbG8=").unwrap(), b"hello");
        assert_eq!(base64_decode("_-8").unwrap(), base64_decode("/+8").unwrap());
        assert!(base64_decode("a$b").is_none());
    }

    #[test]
    fn extension_id_alphabet() {
        let id = extension_id_from_bytes(&[0x00, 0xFF, 0x12, 0x34, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(id.len(), 32);
        assert!(id.starts_with("aappbcde"));
        assert!(is_valid_chrome_extension_id(&id));
        assert!(!is_valid_chrome_extension_id("short"));
        assert!(!is_valid_chrome_extension_id(&"q".repeat(32)));
    }
}
