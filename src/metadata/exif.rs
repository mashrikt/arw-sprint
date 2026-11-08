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
    /// Explicit EXIF Adobe RGB (ColorSpace=2 or interoperability R03).
    pub adobe_rgb: bool,
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
            adobe_rgb: false,
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
                    0x8769 | 0xa005 => {
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
                    0xa001 if !root => metadata.adobe_rgb |= self.scalar(entry) == Some(2),
                    0x0001 if !root => {
                        metadata.adobe_rgb |= self.text(entry)?.as_deref() == Some("R03")
                    }
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

/// Only JPEG headers are inspected. EXIF lives inside one bounded APP1 segment;
/// TIFF offsets remain relative to that segment and cannot escape into pixels.
pub(crate) struct JpegHeader {
    pub exif: ExifMetadata,
    pub exif_tiff: Option<(u64, Vec<u8>)>,
    pub icc_profile: Option<Vec<u8>>,
}

pub(crate) fn read_jpeg_header<R: Read + Seek>(
    source: &mut R,
    length: u64,
) -> Result<JpegHeader, MetadataError> {
    const MAX_HEADER_TRAVEL: u64 = 4 * 1024 * 1024;
    fn at<R: Read + Seek>(
        source: &mut R,
        length: u64,
        offset: u64,
        out: &mut [u8],
    ) -> Result<(), MetadataError> {
        if offset
            .checked_add(out.len() as u64)
            .filter(|end| *end <= length && *end <= MAX_HEADER_TRAVEL)
            .is_none()
        {
            return Err(MetadataError::Invalid("JPEG header range exceeds bounds"));
        }
        source.seek(SeekFrom::Start(offset))?;
        source.read_exact(out)?;
        Ok(())
    }
    let mut start = [0; 2];
    at(source, length, 0, &mut start)?;
    if start != [0xff, 0xd8] {
        return Err(MetadataError::Invalid("JPEG SOI missing"));
    }
    let mut result = JpegHeader {
        exif: ExifMetadata::default(),
        exif_tiff: None,
        icc_profile: None,
    };
    let mut icc_chunks: Vec<(u8, Vec<u8>)> = Vec::new();
    let mut icc_count = 0u8;
    let mut icc_bytes = 0usize;
    let mut offset = 2u64;
    for _ in 0..1024 {
        let mut marker = [0; 2];
        at(source, length, offset, &mut marker)?;
        if marker[0] != 0xff {
            return Err(MetadataError::Invalid("JPEG header marker missing"));
        }
        // Fill bytes count toward the traversal budget, even for malformed files.
        if marker[1] == 0xff {
            offset += 1;
            continue;
        }
        offset += 2;
        if matches!(marker[1], 0xda | 0xd9) {
            if !icc_chunks.is_empty() {
                icc_chunks.sort_unstable_by_key(|(number, _)| *number);
                if icc_chunks.len() != usize::from(icc_count)
                    || icc_chunks
                        .iter()
                        .enumerate()
                        .any(|(i, (number, _))| usize::from(*number) != i + 1)
                {
                    return Err(MetadataError::Invalid(
                        "incomplete or duplicate JPEG ICC profile chunks",
                    ));
                }
                let mut profile = Vec::with_capacity(icc_bytes);
                for (_, chunk) in icc_chunks {
                    profile.extend_from_slice(&chunk);
                }
                result.icc_profile = Some(profile);
            }
            return Ok(result);
        }
        if marker[1] == 0x01 {
            continue;
        }
        if matches!(marker[1], 0 | 0xd0..=0xd8) {
            return Err(MetadataError::Invalid("unexpected JPEG header marker"));
        }
        let mut size = [0; 2];
        at(source, length, offset, &mut size)?;
        let size = u64::from(u16::from_be_bytes(size));
        let end = offset
            .checked_add(size)
            .filter(|end| size >= 2 && *end <= length && *end <= MAX_HEADER_TRAVEL)
            .ok_or(MetadataError::Invalid("JPEG segment outside header"))?;
        if marker[1] == 0xe1 && size >= 8 && result.exif_tiff.is_none() {
            let mut signature = [0; 6];
            at(source, length, offset + 2, &mut signature)?;
            if signature == *b"Exif\0\0" {
                let tiff_offset = offset + 8;
                let mut tiff = vec![0; (size - 8) as usize];
                at(source, length, tiff_offset, &mut tiff)?;
                // Bad EXIF must not prevent viewing otherwise valid JPEG pixels.
                if let Ok(exif) = (MetadataReader {
                    source: io::Cursor::new(&tiff),
                    length: tiff.len() as u64,
                    bytes_read: 0,
                    endian: Endian::Little,
                })
                .parse()
                {
                    result.exif = exif;
                }
                result.exif_tiff = Some((tiff_offset, tiff));
            }
        } else if marker[1] == 0xe2 && size >= 14 {
            let mut signature = [0; 12];
            at(source, length, offset + 2, &mut signature)?;
            if signature == *b"ICC_PROFILE\0" {
                if size < 16 {
                    return Err(MetadataError::Invalid(
                        "truncated JPEG ICC profile sequence",
                    ));
                }
                let mut sequence = [0; 2];
                at(source, length, offset + 14, &mut sequence)?;
                let (number, count) = (sequence[0], sequence[1]);
                if number == 0
                    || count == 0
                    || number > count
                    || (icc_count != 0 && icc_count != count)
                {
                    return Err(MetadataError::Invalid("invalid JPEG ICC profile sequence"));
                }
                icc_count = count;
                icc_bytes = icc_bytes
                    .checked_add((size - 16) as usize)
                    .filter(|n| *n <= 1024 * 1024)
                    .ok_or(MetadataError::Invalid("JPEG ICC profile exceeds 1 MiB"))?;
                let mut chunk = vec![0; (size - 16) as usize];
                at(source, length, offset + 16, &mut chunk)?;
                icc_chunks.push((number, chunk));
            }
        }
        offset = end;
    }
    Err(MetadataError::Invalid("too many JPEG header markers"))
}

/// Read the small display metadata subset by seeking only to IFD0, ExifIFD,
/// and their requested scalar/rational/string values. No image payload reads.
pub fn read_exif(path: &Path) -> Result<ExifMetadata, MetadataError> {
    let mut source = File::open(path)?;
    let stat = source.metadata()?;
    if !stat.is_file() {
        return Err(MetadataError::Invalid("input is not a regular file"));
    }
    let mut signature = [0; 2];
    source.read_exact(&mut signature)?;
    if signature == [0xff, 0xd8] {
        return Ok(read_jpeg_header(&mut source, stat.len())?.exif);
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
    fn jpeg_app(marker: u8, payload: &[u8]) -> Vec<u8> {
        let mut bytes = vec![0xff, marker];
        bytes.extend_from_slice(&((payload.len() + 2) as u16).to_be_bytes());
        bytes.extend_from_slice(payload);
        bytes
    }
    fn jpeg_headers(segments: &[Vec<u8>]) -> Vec<u8> {
        let mut bytes = vec![0xff, 0xd8];
        for segment in segments {
            bytes.extend_from_slice(segment);
        }
        bytes.extend_from_slice(&[0xff, 0xda]);
        bytes
    }
    #[test]
    fn jpeg_app1_reads_orientation_and_metadata_in_both_byte_orders() {
        for big in [false, true] {
            let mut payload = b"Exif\0\0".to_vec();
            payload.extend_from_slice(&fixture(big, 8));
            let bytes = jpeg_headers(&[jpeg_app(0xe1, &payload)]);
            let parsed = read_jpeg_header(&mut Cursor::new(&bytes), bytes.len() as u64).unwrap();
            assert_eq!(parsed.exif.orientation, 8);
            assert_eq!(parsed.exif.camera_model.as_deref(), Some("ILCE-7M5"));
            assert_eq!(parsed.exif.iso, Some(1600));
        }
    }
    #[test]
    fn corrupt_app1_cannot_follow_offsets_into_jpeg_pixels() {
        let mut tiff = fixture(false, 8);
        tiff[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
        let mut payload = b"Exif\0\0".to_vec();
        payload.extend_from_slice(&tiff);
        let bytes = jpeg_headers(&[jpeg_app(0xe1, &payload)]);
        assert_eq!(
            read_jpeg_header(&mut Cursor::new(&bytes), bytes.len() as u64)
                .unwrap()
                .exif
                .orientation,
            1
        );
        for length in 0..bytes.len() {
            assert!(read_jpeg_header(&mut Cursor::new(&bytes[..length]), length as u64).is_err());
        }
    }
    #[test]
    fn icc_chunks_are_ordered_and_missing_or_duplicate_chunks_are_rejected() {
        let chunk = |number, data: &[u8]| {
            let mut payload = b"ICC_PROFILE\0".to_vec();
            payload.extend_from_slice(&[number, 2]);
            payload.extend_from_slice(data);
            jpeg_app(0xe2, &payload)
        };
        let bytes = jpeg_headers(&[chunk(2, b"world"), chunk(1, b"hello")]);
        assert_eq!(
            read_jpeg_header(&mut Cursor::new(&bytes), bytes.len() as u64)
                .unwrap()
                .icc_profile
                .as_deref(),
            Some(b"helloworld".as_slice())
        );
        for segments in [
            vec![chunk(1, b"hello")],
            vec![chunk(1, b"hello"), chunk(1, b"again")],
        ] {
            let bytes = jpeg_headers(&segments);
            assert!(read_jpeg_header(&mut Cursor::new(&bytes), bytes.len() as u64).is_err());
        }
    }
    #[test]
    fn explicit_adobe_rgb_exif_tags_are_recognized() {
        let mut bytes = fixture(false, 1);
        entry(&mut bytes, 130, 0xa001, 3, 1, 2, false);
        assert!(parse_bytes(bytes).unwrap().0.adobe_rgb);
        let mut bytes = fixture(false, 1);
        entry(&mut bytes, 130, 0xa005, 4, 1, 400, false);
        bytes[400..402].copy_from_slice(&1u16.to_le_bytes());
        entry(
            &mut bytes,
            402,
            0x0001,
            2,
            4,
            u32::from_le_bytes(*b"R03\0"),
            false,
        );
        assert!(parse_bytes(bytes).unwrap().0.adobe_rgb);
    }
    #[test]
    fn truncated_icc_sequence_is_not_silently_treated_as_untagged() {
        for suffix in [b"".as_slice(), &[1]] {
            let mut payload = b"ICC_PROFILE\0".to_vec();
            payload.extend_from_slice(suffix);
            let bytes = jpeg_headers(&[jpeg_app(0xe2, &payload)]);
            assert!(read_jpeg_header(&mut Cursor::new(&bytes), bytes.len() as u64).is_err());
        }
    }
}
