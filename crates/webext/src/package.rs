//! Extension packages: `.crx` (CRX2/CRX3), `.xpi` and plain `.zip`.

use crate::encoding::{extension_id_from_bytes, sha256};
use crate::zip::{ZipArchive, ZipError, is_safe_archive_path};
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackageFormat {
    Zip,
    Crx2,
    Crx3,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignatureAlgorithm {
    RsaSha256,
    EcdsaSha256,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrxKeyProof {
    pub algorithm: SignatureAlgorithm,
    /// DER SubjectPublicKeyInfo.
    pub public_key: Vec<u8>,
    pub signature: Vec<u8>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CrxHeader {
    pub version: u32,
    /// Offset of the embedded ZIP archive within the file.
    pub archive_offset: usize,
    pub proofs: Vec<CrxKeyProof>,
    /// The serialized `SignedData` message (CRX3).
    pub signed_header_data: Vec<u8>,
    /// 16 bytes (CRX3).
    pub crx_id: Vec<u8>,
}

impl CrxHeader {
    /// The Chrome extension ID declared by the package.
    pub fn extension_id(&self) -> Option<String> {
        match self.version {
            3 if self.crx_id.len() == 16 => Some(extension_id_from_bytes(&self.crx_id)),
            2 => self.proofs.first().map(|p| extension_id_from_bytes(&sha256(&p.public_key)[..16])),
            _ => None,
        }
    }
}

#[derive(Debug)]
pub enum PackageError {
    Crx(&'static str),
    Zip(ZipError),
    UnsafePath(String),
    Signature(&'static str),
}

impl fmt::Display for PackageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PackageError::Crx(m) => write!(f, "invalid CRX: {m}"),
            PackageError::Zip(e) => write!(f, "invalid archive: {e}"),
            PackageError::UnsafePath(p) => write!(f, "unsafe path in archive: {p}"),
            PackageError::Signature(m) => write!(f, "signature check failed: {m}"),
        }
    }
}

impl std::error::Error for PackageError {}

impl From<ZipError> for PackageError {
    fn from(e: ZipError) -> Self {
        PackageError::Zip(e)
    }
}

/// Verifies a signature. The core has no signature primitives of its own; the embedding browser supplies this (Ladybird
/// has LibCrypto).
pub trait SignatureVerifier {
    fn verify(&self, algorithm: SignatureAlgorithm, public_key: &[u8], signed_data: &[u8], signature: &[u8]) -> bool;
}

impl<F: Fn(SignatureAlgorithm, &[u8], &[u8], &[u8]) -> bool> SignatureVerifier for F {
    fn verify(&self, algorithm: SignatureAlgorithm, public_key: &[u8], signed_data: &[u8], signature: &[u8]) -> bool {
        self(algorithm, public_key, signed_data, signature)
    }
}

struct ProtoReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> ProtoReader<'a> {
    fn at_end(&self) -> bool {
        self.pos >= self.data.len()
    }

    fn varint(&mut self) -> Option<u64> {
        let mut value = 0u64;
        for shift in (0..64).step_by(7) {
            let byte = *self.data.get(self.pos)?;
            self.pos += 1;
            value |= u64::from(byte & 0x7F) << shift;
            if byte & 0x80 == 0 {
                return Some(value);
            }
        }
        None
    }

    fn length_delimited(&mut self) -> Option<&'a [u8]> {
        let len = usize::try_from(self.varint()?).ok()?;
        let end = self.pos.checked_add(len).filter(|&e| e <= self.data.len())?;
        let slice = &self.data[self.pos..end];
        self.pos = end;
        Some(slice)
    }

    fn skip(&mut self, wire_type: u64) -> Option<()> {
        match wire_type {
            0 => {
                self.varint()?;
            }
            1 => self.advance(8)?,
            2 => {
                self.length_delimited()?;
            }
            5 => self.advance(4)?,
            _ => return None,
        }
        Some(())
    }

    fn advance(&mut self, n: usize) -> Option<()> {
        let end = self.pos.checked_add(n).filter(|&e| e <= self.data.len())?;
        self.pos = end;
        Some(())
    }
}

fn parse_key_proof(msg: &[u8], algorithm: SignatureAlgorithm) -> Option<CrxKeyProof> {
    let mut proof = CrxKeyProof { algorithm, public_key: vec![], signature: vec![] };
    let mut r = ProtoReader { data: msg, pos: 0 };
    while !r.at_end() {
        let tag = r.varint()?;
        match (tag >> 3, tag & 7) {
            (1, 2) => proof.public_key = r.length_delimited()?.to_vec(),
            (2, 2) => proof.signature = r.length_delimited()?.to_vec(),
            (_, wire) => r.skip(wire)?,
        }
    }
    Some(proof)
}

fn rd32(d: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_le_bytes(d.get(o..o.checked_add(4)?)?.try_into().ok()?))
}

pub fn parse_crx_header(data: &[u8]) -> Result<CrxHeader, PackageError> {
    use PackageError::Crx;
    if data.len() < 12 || &data[..4] != b"Cr24" {
        return Err(Crx("not a CRX file"));
    }
    let version = rd32(data, 4).ok_or(Crx("truncated"))?;
    let mut header = CrxHeader { version, ..Default::default() };

    match version {
        2 => {
            let key_len = rd32(data, 8).ok_or(Crx("truncated CRX2 header"))? as usize;
            let sig_len = rd32(data, 12).ok_or(Crx("truncated CRX2 header"))? as usize;
            let key_end = 16usize.checked_add(key_len).ok_or(Crx("bad length"))?;
            let end = key_end.checked_add(sig_len).filter(|&e| e <= data.len()).ok_or(Crx("truncated CRX2 header"))?;
            header.proofs.push(CrxKeyProof {
                algorithm: SignatureAlgorithm::RsaSha256,
                public_key: data[16..key_end].to_vec(),
                signature: data[key_end..end].to_vec(),
            });
            header.archive_offset = end;
            Ok(header)
        }
        3 => {
            let header_len = rd32(data, 8).ok_or(Crx("truncated CRX3 header"))? as usize;
            let end = 12usize.checked_add(header_len).filter(|&e| e <= data.len()).ok_or(Crx("truncated CRX3 header"))?;
            header.archive_offset = end;
            let mut r = ProtoReader { data: &data[12..end], pos: 0 };
            while !r.at_end() {
                let tag = r.varint().ok_or(Crx("malformed header"))?;
                match (tag >> 3, tag & 7) {
                    (field @ (2 | 3), 2) => {
                        let msg = r.length_delimited().ok_or(Crx("malformed header"))?;
                        let alg = if field == 2 { SignatureAlgorithm::RsaSha256 } else { SignatureAlgorithm::EcdsaSha256 };
                        header.proofs.push(parse_key_proof(msg, alg).ok_or(Crx("malformed key proof"))?);
                    }
                    (10000, 2) => {
                        let msg = r.length_delimited().ok_or(Crx("malformed header"))?;
                        header.signed_header_data = msg.to_vec();
                        let mut sr = ProtoReader { data: msg, pos: 0 };
                        while !sr.at_end() {
                            let stag = sr.varint().ok_or(Crx("malformed signed data"))?;
                            match (stag >> 3, stag & 7) {
                                (1, 2) => header.crx_id = sr.length_delimited().ok_or(Crx("malformed signed data"))?.to_vec(),
                                (_, wire) => sr.skip(wire).ok_or(Crx("malformed signed data"))?,
                            }
                        }
                    }
                    (_, wire) => r.skip(wire).ok_or(Crx("malformed header"))?,
                }
            }
            if header.crx_id.len() != 16 {
                return Err(Crx("CRX3 header has no valid crx_id"));
            }
            Ok(header)
        }
        _ => Err(Crx("unsupported CRX version")),
    }
}

/// The exact byte string a CRX3 signature covers.
pub fn crx3_signed_data(header: &CrxHeader, archive: &[u8]) -> Vec<u8> {
    let mut out = b"CRX3 SignedData\0".to_vec();
    out.extend_from_slice(&(header.signed_header_data.len() as u32).to_le_bytes());
    out.extend_from_slice(&header.signed_header_data);
    out.extend_from_slice(archive);
    out
}

/// Verifies a CRX3 package. Every proof must verify, and one proof's key must hash to the declared `crx_id`.
pub fn verify_crx3(header: &CrxHeader, archive: &[u8], verifier: &dyn SignatureVerifier) -> Result<(), PackageError> {
    if header.version != 3 {
        return Err(PackageError::Signature("not a CRX3 package"));
    }
    if header.proofs.is_empty() {
        return Err(PackageError::Signature("no signatures"));
    }
    let signed = crx3_signed_data(header, archive);
    let mut id_matched = false;
    for proof in &header.proofs {
        if !verifier.verify(proof.algorithm, &proof.public_key, &signed, &proof.signature) {
            return Err(PackageError::Signature("signature verification failed"));
        }
        if sha256(&proof.public_key)[..16] == header.crx_id[..] {
            id_matched = true;
        }
    }
    if id_matched { Ok(()) } else { Err(PackageError::Signature("no signing key matches the declared extension ID")) }
}

/// A parsed extension package.
#[derive(Debug)]
pub struct Package {
    format: PackageFormat,
    crx: Option<CrxHeader>,
    archive: ZipArchive,
}

impl Package {
    /// Parses a package and rejects any archive containing unsafe paths.
    pub fn open(data: Vec<u8>) -> Result<Package, PackageError> {
        let (format, crx, zip_bytes) = if data.starts_with(b"Cr24") {
            let header = parse_crx_header(&data)?;
            let format = if header.version == 3 { PackageFormat::Crx3 } else { PackageFormat::Crx2 };
            let zip_bytes = data[header.archive_offset..].to_vec();
            (format, Some(header), zip_bytes)
        } else {
            (PackageFormat::Zip, None, data)
        };
        let archive = ZipArchive::open(zip_bytes)?;
        if let Some(bad) = archive.entries().iter().find(|e| !is_safe_archive_path(&e.name)) {
            return Err(PackageError::UnsafePath(bad.name.clone()));
        }
        Ok(Package { format, crx, archive })
    }

    pub fn format(&self) -> PackageFormat {
        self.format
    }

    pub fn crx_header(&self) -> Option<&CrxHeader> {
        self.crx.as_ref()
    }

    pub fn archive(&self) -> &ZipArchive {
        &self.archive
    }

    pub fn has_file(&self, path: &str) -> bool {
        self.archive.find(path).is_some()
    }

    pub fn read_file(&self, path: &str) -> Result<Vec<u8>, ZipError> {
        self.archive.read(path)
    }

    pub fn read_text(&self, path: &str) -> Option<String> {
        self.archive.read(path).ok().map(|b| String::from_utf8_lossy(&b).into_owned())
    }

    /// Verifies the CRX3 signature over the embedded archive. Plain zip/xpi packages have no CRX signature.
    pub fn verify_crx3(&self, verifier: &dyn SignatureVerifier) -> Result<(), PackageError> {
        match &self.crx {
            Some(h) if h.version == 3 => verify_crx3(h, self.archive.bytes(), verifier),
            _ => Err(PackageError::Signature("not a CRX3 package")),
        }
    }

    /// True if META-INF carries Mozilla's PKCS#7 signature files, as on signed `.xpi` files. Cryptographic verification is
    /// left to the host.
    pub fn has_mozilla_signature(&self) -> bool {
        self.has_file("META-INF/mozilla.rsa") && self.has_file("META-INF/mozilla.sf") && self.has_file("META-INF/manifest.mf")
    }

    /// The add-on ID addons.mozilla.org signed the package for: the common name of the signing certificate in
    /// `META-INF/mozilla.rsa`. AMO signs add-ons whose manifest declares no gecko ID under an ID it assigns, and Firefox
    /// then uses that ID. The signature itself is not verified here.
    pub fn mozilla_signed_id(&self) -> Option<String> {
        if !self.has_mozilla_signature() {
            return None;
        }
        mozilla_signed_id_from_pkcs7(&self.read_file("META-INF/mozilla.rsa").ok()?)
    }
}

/// Finds the add-on ID among the common names in a PKCS#7 blob. The signing chain also names Mozilla's intermediate
/// and root certificates; the add-on certificate's common name is the one that looks like an add-on ID
/// (`name@domain` or `{uuid}`).
pub fn mozilla_signed_id_from_pkcs7(der: &[u8]) -> Option<String> {
    const COMMON_NAME_OID: [u8; 5] = [0x06, 0x03, 0x55, 0x04, 0x03];
    let looks_like_addon_id = |s: &str| {
        let email = s.contains('@') && !s.contains(char::is_whitespace);
        let uuid = s.len() == 38 && s.starts_with('{') && s.ends_with('}');
        email || uuid
    };
    let mut i = 0;
    while i + COMMON_NAME_OID.len() + 2 <= der.len() {
        if der[i..i + COMMON_NAME_OID.len()] != COMMON_NAME_OID {
            i += 1;
            continue;
        }
        let tag = der[i + 5];
        let len = der[i + 6] as usize;
        let start = i + 7;
        // UTF8String, PrintableString or IA5String with a short-form length.
        if matches!(tag, 0x0c | 0x13 | 0x16)
            && len < 0x80
            && start + len <= der.len()
            && let Ok(name) = std::str::from_utf8(&der[start..start + len])
            && looks_like_addon_id(name)
        {
            return Some(name.to_string());
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::zip::testutil::build_zip;

    fn varint(mut v: u64) -> Vec<u8> {
        let mut out = vec![];
        loop {
            let b = (v & 0x7F) as u8;
            v >>= 7;
            if v == 0 {
                out.push(b);
                return out;
            }
            out.push(b | 0x80);
        }
    }

    fn field(num: u64, payload: &[u8]) -> Vec<u8> {
        let mut out = varint(num << 3 | 2);
        out.extend(varint(payload.len() as u64));
        out.extend_from_slice(payload);
        out
    }

    fn build_crx3(public_key: &[u8], signature: &[u8], zip: &[u8]) -> (Vec<u8>, Vec<u8>) {
        let crx_id = sha256(public_key)[..16].to_vec();
        let signed_data = field(1, &crx_id);
        let mut proof = field(1, public_key);
        proof.extend(field(2, signature));
        let mut header = field(2, &proof);
        header.extend(field(10000, &signed_data));
        let mut file = b"Cr24".to_vec();
        file.extend_from_slice(&3u32.to_le_bytes());
        file.extend_from_slice(&(header.len() as u32).to_le_bytes());
        file.extend(header);
        file.extend_from_slice(zip);
        (file, crx_id)
    }

    #[test]
    fn crx3_parse_id_and_verify() {
        let zip = build_zip(&[("manifest.json", br#"{"manifest_version":3}"#)], true);
        let key = b"fake-public-key-der";
        let (file, crx_id) = build_crx3(key, b"sig", &zip);

        let pkg = Package::open(file).unwrap();
        assert_eq!(pkg.format(), PackageFormat::Crx3);
        let header = pkg.crx_header().unwrap();
        assert_eq!(header.crx_id, crx_id);
        assert_eq!(header.extension_id().unwrap(), crate::encoding::chrome_extension_id_from_public_key(key));
        assert!(pkg.read_text("manifest.json").unwrap().contains("manifest_version"));

        // The verifier must be handed exactly the data a real CRX3 signature covers.
        let expected = crx3_signed_data(header, &zip);
        let ok = |_: SignatureAlgorithm, k: &[u8], data: &[u8], sig: &[u8]| k == key && sig == b"sig" && data == expected.as_slice();
        assert!(pkg.verify_crx3(&ok).is_ok());
        let bad = |_: SignatureAlgorithm, _: &[u8], _: &[u8], _: &[u8]| false;
        assert!(pkg.verify_crx3(&bad).is_err());
    }

    #[test]
    fn crx3_with_mismatched_id_is_rejected() {
        let zip = build_zip(&[("manifest.json", b"{}")], false);
        let (mut file, _) = build_crx3(b"key", b"sig", &zip);
        // Corrupt the crx_id bytes inside the header (they follow the 0x0A 0x10 tag/length of SignedData.crx_id).
        let pos = file.windows(2).position(|w| w == [0x0A, 0x10]).unwrap() + 2;
        file[pos] ^= 0xFF;
        let pkg = Package::open(file).unwrap();
        let yes = |_: SignatureAlgorithm, _: &[u8], _: &[u8], _: &[u8]| true;
        assert!(pkg.verify_crx3(&yes).is_err());
    }

    #[test]
    fn plain_zip_and_xpi_detection() {
        let zip = build_zip(
            &[("manifest.json", b"{}"), ("META-INF/mozilla.rsa", b"x"), ("META-INF/mozilla.sf", b"x"), ("META-INF/manifest.mf", b"x")],
            false,
        );
        let pkg = Package::open(zip).unwrap();
        assert_eq!(pkg.format(), PackageFormat::Zip);
        assert!(pkg.has_mozilla_signature());
        assert!(pkg.verify_crx3(&|_: SignatureAlgorithm, _: &[u8], _: &[u8], _: &[u8]| true).is_err());
    }

    #[test]
    fn rejects_path_traversal() {
        let zip = build_zip(&[("manifest.json", b"{}"), ("../evil.js", b"x")], false);
        assert!(matches!(Package::open(zip), Err(PackageError::UnsafePath(_))));
    }

    #[test]
    fn rejects_malformed_crx() {
        assert!(Package::open(b"Cr24".to_vec()).is_err());
        let mut f = b"Cr24".to_vec();
        f.extend_from_slice(&3u32.to_le_bytes());
        f.extend_from_slice(&1000u32.to_le_bytes());
        assert!(Package::open(f).is_err());
        let mut v9 = b"Cr24".to_vec();
        v9.extend_from_slice(&9u32.to_le_bytes());
        v9.extend_from_slice(&0u32.to_le_bytes());
        assert!(Package::open(v9).is_err());
    }
}
