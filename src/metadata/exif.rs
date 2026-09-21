use std::{
    collections::{HashSet, VecDeque},
    fmt,
    fs::File,
    io::{self, Read, Seek, SeekFrom},
    path::Path,
};

const MAX_METADATA_BYTES: usize = 256 * 1024;
const MAX_ENTRIES: usize = 1024;
const MAX_IFDS: usize = 16;
const MAX_TEXT_BYTES: usize = 512;

#[derive(Clone, Debug)]
pub struct ExifMetadata {
    pub orientation: u16,
    pub exposure_time: Option<f64>,
    pub aperture: Option<f64>,
    pub iso: Option<u32>,
    pub focal_length: Option<f64>,
    pub camera_model: Option<String>,
    pub lens_model: Option<String>,
    pub captured_at: Option<String>,
}

impl Default for ExifMetadata {
    fn default() -> Self {
        Self {
            orientation: 1,
            exposure_time: None,
            aperture: None,
            iso: None,
            focal_length: None,
            camera_model: None,
            lens_model: None,
            captured_at: None,
        }
    }
}

#[derive(Debug)]
pub enum MetadataError {
    Io(io::Error),
    Invalid(&'static str),
}
impl fmt::Display for MetadataError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "EXIF I/O: {error}"),
            Self::Invalid(reason) => write!(f, "EXIF unavailable: {reason}"),
        }
    }
}
impl std::error::Error for MetadataError {}
impl From<io::Error> for MetadataError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

#[derive(Clone, Copy)]
enum Endian {
    Little,
    Big,
}
impl Endian {
    fn u16(self, bytes: &[u8]) -> u16 {
        let value = [bytes[0], bytes[1]];
        match self {
            Self::Little => u16::from_le_bytes(value),
            Self::Big => u16::from_be_bytes(value),
        }
    }
    fn u32(self, bytes: &[u8]) -> u32 {
        let value = [bytes[0], bytes[1], bytes[2], bytes[3]];
        match self {
            Self::Little => u32::from_le_bytes(value),
            Self::Big => u32::from_be_bytes(value),
        }
    }
}

struct MetadataReader<R> {
    source: R,
    length: u64,
    bytes_read: usize,
    endian: Endian,
}
impl<R: Read + Seek> MetadataReader<R> {
    fn read(&mut self, offset: u64, bytes: &mut [u8]) -> Result<(), MetadataError> {
        if offset
            .checked_add(bytes.len() as u64)
            .filter(|end| *end <= self.length)
            .is_none()
        {
            return Err(MetadataError::Invalid("TIFF byte range outside file"));
        }
        self.bytes_read = self
            .bytes_read
            .checked_add(bytes.len())
            .filter(|n| *n <= MAX_METADATA_BYTES)
            .ok_or(MetadataError::Invalid("metadata read budget exceeded"))?;
        self.source.seek(SeekFrom::Start(offset))?;
        self.source.read_exact(bytes)?;
        Ok(())
    }
    fn scalar(&self, entry: &[u8]) -> Option<u32> {
        if self.endian.u32(&entry[4..8]) != 1 {
            return None;
        }
        match self.endian.u16(&entry[2..4]) {
            3 => Some(u32::from(self.endian.u16(&entry[8..10]))),
            4 => Some(self.endian.u32(&entry[8..12])),
            _ => None,
        }
    }
    fn rational(&mut self, entry: &[u8]) -> Result<Option<f64>, MetadataError> {
        if self.endian.u16(&entry[2..4]) != 5 || self.endian.u32(&entry[4..8]) != 1 {
            return Ok(None);
        }
        let mut value = [0; 8];
        self.read(u64::from(self.endian.u32(&entry[8..12])), &mut value)?;
        let numerator = self.endian.u32(&value[..4]);
        let denominator = self.endian.u32(&value[4..]);
        Ok((denominator != 0 && numerator != 0)
            .then(|| f64::from(numerator) / f64::from(denominator)))
    }
    fn text(&mut self, entry: &[u8]) -> Result<Option<String>, MetadataError> {
        if self.endian.u16(&entry[2..4]) != 2 {
            return Ok(None);
        }
        let length = self.endian.u32(&entry[4..8]) as usize;
        if length == 0 || length > MAX_TEXT_BYTES {
            return Ok(None);
        }
        let mut bytes = vec![0; length];
        if length <= 4 {
            bytes.copy_from_slice(&entry[8..8 + length]);
        } else {
            self.read(u64::from(self.endian.u32(&entry[8..12])), &mut bytes)?;
        }
        let end = bytes
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(bytes.len());
        if bytes[..end]
            .iter()
            .any(|byte| *byte < 32 && !matches!(byte, b'\t' | b'\n' | b'\r'))
        {
            return Ok(None);
        }
        let text = String::from_utf8_lossy(&bytes[..end]).trim().to_owned();
        Ok((!text.is_empty()).then_some(text))
    }
    fn parse(&mut self) -> Result<ExifMetadata, MetadataError> {
        let mut header = [0; 8];
        self.read(0, &mut header)?;
        self.endian = match &header[..2] {
            b"II" => Endian::Little,
            b"MM" => Endian::Big,
            _ => return Err(MetadataError::Invalid("missing TIFF byte-order header")),
        };
        if self.endian.u16(&header[2..4]) != 42 {
            return Err(MetadataError::Invalid("unsupported TIFF container"));
        }
        let first = u64::from(self.endian.u32(&header[4..8]));
        if first < 8 {
            return Err(MetadataError::Invalid("invalid first IFD offset"));
        }
        let mut queue = VecDeque::from([(first, true)]);
        let mut visited = HashSet::new();
        let mut metadata = ExifMetadata::default();
        let mut original_time = None;
        while let Some((offset, root)) = queue.pop_front() {
            if offset == 0 || !visited.insert(offset) {
                continue;
            }
            if visited.len() > MAX_IFDS {
                return Err(MetadataError::Invalid("too many EXIF directories"));
            }
            let mut count = [0; 2];
            self.read(offset, &mut count)?;
            let count = self.endian.u16(&count) as usize;
            if count > MAX_ENTRIES {
                return Err(MetadataError::Invalid("too many EXIF entries"));
            }
            let byte_length = count
                .checked_mul(12)
                .ok_or(MetadataError::Invalid("IFD size overflow"))?;
            let mut bytes = vec![0; byte_length];
            self.read(
                offset
                    .checked_add(2)
                    .ok_or(MetadataError::Invalid("IFD offset overflow"))?,
                &mut bytes,
            )?;
            for entry in bytes.chunks_exact(12) {
                let tag = self.endian.u16(&entry[..2]);
                match tag {
                    0x0112 if root => {
                        if let Some(value @ 1..=8) = self.scalar(entry) {
                            metadata.orientation = value as u16;
                        }
                    }
                    0x0110 if root => metadata.camera_model = self.text(entry)?,
                    0x0132 if root => metadata.captured_at = self.text(entry)?,
                    0x8769 => {
                        if let Some(pointer) = self.scalar(entry) {
                            if queue.len() >= MAX_IFDS {
                                return Err(MetadataError::Invalid("too many EXIF pointers"));
                            }
                            queue.push_back((u64::from(pointer), false));
                        }
                    }
                    0x829a if !root => metadata.exposure_time = self.rational(entry)?,
                    0x829d if !root => metadata.aperture = self.rational(entry)?,
                    0x8827 if !root => {
                        metadata.iso = self.scalar(entry).filter(|value| *value != 0)
                    }
                    0x8833 if !root && metadata.iso.is_none() => {
                        metadata.iso = self.scalar(entry).filter(|value| *value != 0)
                    }
                    0x920a if !root => metadata.focal_length = self.rational(entry)?,
                    0x9003 if !root => original_time = self.text(entry)?,
                    0xa434 if !root => metadata.lens_model = self.text(entry)?,
                    _ => {} // Never follow sensor SubIFDs, thumbnail chains, or MakerNotes.
                }
            }
        }
        if original_time.is_some() {
            metadata.captured_at = original_time;
        }
        Ok(metadata)
    }
}

/// Read the small display metadata subset by seeking only to IFD0, ExifIFD,
/// and their requested scalar/rational/string values. No image payload reads.
pub fn read_exif(path: &Path) -> Result<ExifMetadata, MetadataError> {
    let source = File::open(path)?;
    let stat = source.metadata()?;
    if !stat.is_file() {
        return Err(MetadataError::Invalid("input is not a regular file"));
    }
    MetadataReader {
        source,
        length: stat.len(),
        bytes_read: 0,
        endian: Endian::Little,
    }
    .parse()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn value16(value: u16, big: bool) -> [u8; 2] {
        if big {
            value.to_be_bytes()
        } else {
            value.to_le_bytes()
        }
    }
    fn value32(value: u32, big: bool) -> [u8; 4] {
        if big {
            value.to_be_bytes()
        } else {
            value.to_le_bytes()
        }
    }
    fn entry(bytes: &mut [u8], at: usize, tag: u16, kind: u16, count: u32, value: u32, big: bool) {
        bytes[at..at + 2].copy_from_slice(&value16(tag, big));
        bytes[at + 2..at + 4].copy_from_slice(&value16(kind, big));
        bytes[at + 4..at + 8].copy_from_slice(&value32(count, big));
        if kind == 3 && count == 1 {
            bytes[at + 8..at + 10].copy_from_slice(&value16(value as u16, big));
        } else {
            bytes[at + 8..at + 12].copy_from_slice(&value32(value, big));
        }
    }
    fn fixture(big: bool, orientation: u16) -> Vec<u8> {
        let mut bytes = vec![0; 512];
        bytes[..2].copy_from_slice(if big { b"MM" } else { b"II" });
        bytes[2..4].copy_from_slice(&value16(42, big));
        bytes[4..8].copy_from_slice(&value32(8, big));
        bytes[8..10].copy_from_slice(&value16(4, big));
        entry(&mut bytes, 10, 0x0112, 3, 1, u32::from(orientation), big);
        entry(&mut bytes, 22, 0x0110, 2, 9, 256, big);
        entry(&mut bytes, 34, 0x8769, 4, 1, 80, big);
        // A poisonous sensor pointer must be ignored.
        entry(&mut bytes, 46, 0x014a, 4, 1, u32::MAX, big);
        bytes[80..82].copy_from_slice(&value16(6, big));
        entry(&mut bytes, 82, 0x829a, 5, 1, 280, big);
        entry(&mut bytes, 94, 0x829d, 5, 1, 288, big);
        entry(&mut bytes, 106, 0x8827, 3, 1, 1600, big);
        entry(&mut bytes, 118, 0x920a, 5, 1, 296, big);
        entry(&mut bytes, 130, 0xa434, 2, 10, 320, big);
        entry(&mut bytes, 142, 0x9003, 2, 20, 340, big);
        bytes[256..265].copy_from_slice(b"ILCE-7M5\0");
        for (at, numerator, denominator) in [(280, 1, 2000), (288, 63, 10), (296, 600, 1)] {
            bytes[at..at + 4].copy_from_slice(&value32(numerator, big));
            bytes[at + 4..at + 8].copy_from_slice(&value32(denominator, big));
        }
        bytes[320..330].copy_from_slice(b"Test lens\0");
        bytes[340..360].copy_from_slice(b"2026:01:02 03:04:05\0");
        bytes
    }
    fn parse_bytes(bytes: Vec<u8>) -> Result<(ExifMetadata, usize), MetadataError> {
        let mut reader = MetadataReader {
            length: bytes.len() as u64,
            source: Cursor::new(bytes),
            bytes_read: 0,
            endian: Endian::Little,
        };
        let data = reader.parse()?;
        Ok((data, reader.bytes_read))
    }
    #[test]
    fn both_endians_rationals_and_all_eight_orientations() {
        for big in [false, true] {
            for orientation in 1..=8 {
                let (data, reads) = parse_bytes(fixture(big, orientation)).unwrap();
                assert_eq!(data.orientation, orientation);
                assert_eq!(data.exposure_time, Some(0.0005));
                assert_eq!(data.aperture, Some(6.3));
                assert_eq!(data.iso, Some(1600));
                assert_eq!(data.focal_length, Some(600.0));
                assert_eq!(data.camera_model.as_deref(), Some("ILCE-7M5"));
                assert_eq!(data.lens_model.as_deref(), Some("Test lens"));
                assert_eq!(data.captured_at.as_deref(), Some("2026:01:02 03:04:05"));
                assert!(reads < 256);
            }
        }
    }
    #[test]
    fn invalid_orientation_defaults_to_one_and_zero_denominator_is_absent() {
        let mut bytes = fixture(false, 0);
        bytes[284..288].fill(0);
        let (data, _) = parse_bytes(bytes).unwrap();
        assert_eq!(data.orientation, 1);
        assert_eq!(data.exposure_time, None);
        assert_eq!(ExifMetadata::default().orientation, 1);
    }
    #[test]
    fn truncated_offsets_and_huge_counts_do_not_allocate_or_panic() {
        let mut bytes = fixture(false, 1);
        bytes[90..94].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(parse_bytes(bytes).is_err());
        let mut bytes = fixture(false, 1);
        bytes[8..10].copy_from_slice(&u16::MAX.to_le_bytes());
        assert!(parse_bytes(bytes).is_err());
        let bytes = fixture(false, 1);
        for n in 0..154 {
            assert!(parse_bytes(bytes[..n].to_vec()).is_err());
        }
    }
}
