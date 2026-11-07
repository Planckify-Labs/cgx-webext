//! A minimal, bounds-checked, read-only ZIP reader. No ZIP64, encryption or multi-disk support, none of which are valid
//! in extension packages.

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZipEntry {
    pub name: String,
    pub method: u16,
    pub flags: u16,
    pub crc32: u32,
    pub compressed_size: u32,
    pub uncompressed_size: u32,
    pub local_header_offset: u32,
}

impl ZipEntry {
    pub fn is_directory(&self) -> bool {
        self.name.ends_with('/')
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ZipError {
    TooSmall,
    NoEndOfCentralDirectory,
    Zip64Unsupported,
    Corrupt(&'static str),
    NotFound,
    Encrypted,
    TooLarge,
    UnsupportedMethod(u16),
    BadChecksum,
}

impl fmt::Display for ZipError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ZipError::TooSmall => write!(f, "archive too small"),
            ZipError::NoEndOfCentralDirectory => write!(f, "end of central directory not found"),
            ZipError::Zip64Unsupported => write!(f, "ZIP64 is not supported"),
            ZipError::Corrupt(what) => write!(f, "corrupt archive: {what}"),
            ZipError::NotFound => write!(f, "entry not found"),
            ZipError::Encrypted => write!(f, "encrypted entries are not supported"),
            ZipError::TooLarge => write!(f, "entry exceeds the size limit"),
            ZipError::UnsupportedMethod(m) => write!(f, "unsupported compression method {m}"),
            ZipError::BadChecksum => write!(f, "CRC mismatch"),
        }
    }
}

impl std::error::Error for ZipError {}

pub const DEFAULT_MAX_ENTRY_SIZE: usize = 64 * 1024 * 1024;

/// Whether an archive entry name can safely be mapped onto a directory: relative, no `..` components, no backslashes, no
/// NULs and no drive letters.
pub fn is_safe_archive_path(name: &str) -> bool {
    if name.is_empty() || name.starts_with('/') || name.contains('\\') || name.contains('\0') {
        return false;
    }
    let bytes = name.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' {
        return false;
    }
    !name.split('/').any(|part| part == "..")
}

fn rd16(d: &[u8], o: usize) -> Option<u16> {
    Some(u16::from_le_bytes(d.get(o..o.checked_add(2)?)?.try_into().ok()?))
}

fn rd32(d: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_le_bytes(d.get(o..o.checked_add(4)?)?.try_into().ok()?))
}

/// An archive that owns its bytes.
#[derive(Debug, Clone)]
pub struct ZipArchive {
    data: Vec<u8>,
    entries: Vec<ZipEntry>,
}

impl ZipArchive {
    pub fn open(data: Vec<u8>) -> Result<Self, ZipError> {
        if data.len() < 22 {
            return Err(ZipError::TooSmall);
        }
        let search_start = data.len().saturating_sub(22 + 65535);
        let eocd = (search_start..=data.len() - 22)
            .rev()
            .find(|&i| rd32(&data, i) == Some(0x0605_4b50))
            .ok_or(ZipError::NoEndOfCentralDirectory)?;

        let corrupt = ZipError::Corrupt("truncated end of central directory");
        let total = rd16(&data, eocd + 10).ok_or(corrupt.clone())?;
        let cd_size = rd32(&data, eocd + 12).ok_or(corrupt.clone())?;
        let cd_offset = rd32(&data, eocd + 16).ok_or(corrupt)?;
        if total == 0xFFFF || cd_size == 0xFFFF_FFFF || cd_offset == 0xFFFF_FFFF {
            return Err(ZipError::Zip64Unsupported);
        }
        if cd_offset as u64 + cd_size as u64 > data.len() as u64 {
            return Err(ZipError::Corrupt("central directory out of bounds"));
        }

        let bad = ZipError::Corrupt("central directory entry");
        let mut entries = Vec::with_capacity(total as usize);
        let mut pos = cd_offset as usize;
        for _ in 0..total {
            if rd32(&data, pos) != Some(0x0201_4b50) {
                return Err(bad);
            }
            let flags = rd16(&data, pos + 8).ok_or(bad.clone())?;
            let method = rd16(&data, pos + 10).ok_or(bad.clone())?;
            let crc32 = rd32(&data, pos + 16).ok_or(bad.clone())?;
            let compressed_size = rd32(&data, pos + 20).ok_or(bad.clone())?;
            let uncompressed_size = rd32(&data, pos + 24).ok_or(bad.clone())?;
            let name_len = rd16(&data, pos + 28).ok_or(bad.clone())? as usize;
            let extra_len = rd16(&data, pos + 30).ok_or(bad.clone())? as usize;
            let comment_len = rd16(&data, pos + 32).ok_or(bad.clone())? as usize;
            let local_header_offset = rd32(&data, pos + 42).ok_or(bad.clone())?;
            let name_bytes = data.get(pos + 46..pos + 46 + name_len).ok_or(bad.clone())?;
            let name = String::from_utf8_lossy(name_bytes).into_owned();
            pos = pos.checked_add(46 + name_len + extra_len + comment_len).filter(|&p| p <= data.len()).ok_or(bad.clone())?;
            entries.push(ZipEntry { name, method, flags, crc32, compressed_size, uncompressed_size, local_header_offset });
        }
        Ok(ZipArchive { data, entries })
    }

    pub fn entries(&self) -> &[ZipEntry] {
        &self.entries
    }

    pub fn find(&self, name: &str) -> Option<&ZipEntry> {
        self.entries.iter().find(|e| e.name == name)
    }

    pub fn bytes(&self) -> &[u8] {
        &self.data
    }

    /// Extracts and CRC-checks an entry. `max_size` bounds the output to defeat decompression bombs.
    pub fn read_entry(&self, entry: &ZipEntry, max_size: usize) -> Result<Vec<u8>, ZipError> {
        if entry.flags & 1 != 0 {
            return Err(ZipError::Encrypted);
        }
        if entry.uncompressed_size as usize > max_size {
            return Err(ZipError::TooLarge);
        }
        let lho = entry.local_header_offset as usize;
        let c = ZipError::Corrupt("local header");
        if rd32(&self.data, lho) != Some(0x0403_4b50) {
            return Err(c);
        }
        let name_len = rd16(&self.data, lho + 26).ok_or(c.clone())? as usize;
        let extra_len = rd16(&self.data, lho + 28).ok_or(c)? as usize;
        let start = lho + 30 + name_len + extra_len;
        let end = start
            .checked_add(entry.compressed_size as usize)
            .filter(|&e| e <= self.data.len())
            .ok_or(ZipError::Corrupt("entry data out of bounds"))?;
        let payload = &self.data[start..end];

        let out = match entry.method {
            0 => payload.to_vec(),
            // Bounded by the declared size so a lying header cannot cause a bomb.
            8 => miniz_oxide::inflate::decompress_to_vec_with_limit(payload, entry.uncompressed_size as usize)
                .map_err(|_| ZipError::Corrupt("deflate stream"))?,
            m => return Err(ZipError::UnsupportedMethod(m)),
        };
        if out.len() != entry.uncompressed_size as usize {
            return Err(ZipError::Corrupt("size mismatch"));
        }
        if crc32fast::hash(&out) != entry.crc32 {
            return Err(ZipError::BadChecksum);
        }
        Ok(out)
    }

    pub fn read(&self, name: &str) -> Result<Vec<u8>, ZipError> {
        let entry = self.find(name).ok_or(ZipError::NotFound)?;
        self.read_entry(entry, DEFAULT_MAX_ENTRY_SIZE)
    }
}

#[cfg(test)]
pub(crate) mod testutil {
    /// Builds a ZIP file in memory (stored or deflated) for tests.
    pub fn build_zip(files: &[(&str, &[u8])], deflate: bool) -> Vec<u8> {
        let mut out = Vec::new();
        let mut central = Vec::new();
        for (name, content) in files {
            let crc = crc32fast::hash(content);
            let (method, payload) =
                if deflate { (8u16, miniz_oxide::deflate::compress_to_vec(content, 6)) } else { (0u16, content.to_vec()) };
            let offset = out.len() as u32;
            out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
            out.extend_from_slice(&[20, 0, 0, 0]);
            out.extend_from_slice(&method.to_le_bytes());
            out.extend_from_slice(&[0, 0, 0, 0]);
            out.extend_from_slice(&crc.to_le_bytes());
            out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            out.extend_from_slice(&(content.len() as u32).to_le_bytes());
            out.extend_from_slice(&(name.len() as u16).to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(&payload);

            central.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
            central.extend_from_slice(&[20, 0, 20, 0, 0, 0]);
            central.extend_from_slice(&method.to_le_bytes());
            central.extend_from_slice(&[0, 0, 0, 0]);
            central.extend_from_slice(&crc.to_le_bytes());
            central.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            central.extend_from_slice(&(content.len() as u32).to_le_bytes());
            central.extend_from_slice(&(name.len() as u16).to_le_bytes());
            central.extend_from_slice(&[0; 12]);
            central.extend_from_slice(&offset.to_le_bytes());
            central.extend_from_slice(name.as_bytes());
        }
        let cd_offset = out.len() as u32;
        out.extend_from_slice(&central);
        out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
        out.extend_from_slice(&[0, 0, 0, 0]);
        out.extend_from_slice(&(files.len() as u16).to_le_bytes());
        out.extend_from_slice(&(files.len() as u16).to_le_bytes());
        out.extend_from_slice(&(central.len() as u32).to_le_bytes());
        out.extend_from_slice(&cd_offset.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out
    }
}

#[cfg(test)]
mod tests {
    use super::testutil::build_zip;
    use super::*;

    #[test]
    fn roundtrip_stored_and_deflated() {
        let text = b"hello hello hello hello hello world".repeat(20);
        for deflate in [false, true] {
            let zip = build_zip(&[("a.txt", &text), ("dir/b.txt", b"b")], deflate);
            let archive = ZipArchive::open(zip).unwrap();
            assert_eq!(archive.entries().len(), 2);
            assert_eq!(archive.read("a.txt").unwrap(), text);
            assert_eq!(archive.read("dir/b.txt").unwrap(), b"b");
            assert_eq!(archive.read("missing"), Err(ZipError::NotFound));
        }
    }

    #[test]
    fn detects_corruption() {
        let mut zip = build_zip(&[("a.txt", b"payload payload payload")], false);
        let pos = zip.windows(7).position(|w| w == b"payload").unwrap();
        zip[pos] ^= 0xFF;
        let archive = ZipArchive::open(zip).unwrap();
        assert_eq!(archive.read("a.txt"), Err(ZipError::BadChecksum));
    }

    #[test]
    fn rejects_garbage_and_truncation() {
        assert!(ZipArchive::open(vec![0; 10]).is_err());
        assert!(ZipArchive::open(vec![0x50; 100]).is_err());
        let zip = build_zip(&[("a", b"1")], false);
        assert!(ZipArchive::open(zip[..zip.len() - 5].to_vec()).is_err());
    }

    #[test]
    fn decompression_bomb_is_bounded() {
        let big = vec![0u8; 1_000_000];
        let mut zip = build_zip(&[("z", &big)], true);
        // Lie about the uncompressed size in both headers (local at +22, central after).
        let local = 22;
        zip[local..local + 4].copy_from_slice(&10u32.to_le_bytes());
        let cd = zip.windows(4).rposition(|w| w == 0x0201_4b50u32.to_le_bytes()).unwrap();
        zip[cd + 24..cd + 28].copy_from_slice(&10u32.to_le_bytes());
        let archive = ZipArchive::open(zip).unwrap();
        assert!(archive.read("z").is_err());
    }

    #[test]
    fn safe_paths() {
        assert!(is_safe_archive_path("manifest.json"));
        assert!(is_safe_archive_path("a/b/c.js"));
        assert!(is_safe_archive_path("a..b/c"));
        assert!(!is_safe_archive_path("../x"));
        assert!(!is_safe_archive_path("a/../../x"));
        assert!(!is_safe_archive_path("/etc/passwd"));
        assert!(!is_safe_archive_path("a\\b"));
        assert!(!is_safe_archive_path("C:evil"));
        assert!(!is_safe_archive_path(""));
    }
}
