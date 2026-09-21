use super::{
    jpeg,
    tiff::{checked_range, Endian, Entry},
};
use std::{
    collections::{HashSet, VecDeque},
    fmt,
    fs::File,
    io::{self, Read, Seek, SeekFrom},
    path::Path,
};

const MAX_IFDS: usize = 128;
const MAX_ENTRIES: usize = 4096;
const MAX_POINTERS: usize = 128;
const MAX_CANDIDATES: usize = 128;
const MAX_METADATA: u64 = 4 * 1024 * 1024;
pub const MAX_JPEG_BYTES: u64 = 128 * 1024 * 1024;

#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    Invalid(&'static str),
    Limit(&'static str),
    NoPreview,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "{e}"),
            Self::Invalid(s) => write!(f, "malformed or unsupported ARW/JPEG: {s}"),
            Self::Limit(s) => write!(f, "input safety limit: {s}"),
            Self::NoPreview => write!(f, "no usable embedded JPEG preview found"),
        }
    }
}
impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}
impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EmbeddedPreview {
    pub offset: u64,
    pub length: u64,
    pub width: Option<u32>,
    pub height: Option<u32>,
}
#[derive(Clone, Copy, Debug, Default)]
pub struct IoStats {
    /// Bytes requested by bounded read calls, including failed/partial calls.
    /// This is application I/O, not a measurement of physical disk traffic.
    pub bytes_read: u64,
    /// Logical read requests, not the number of kernel syscalls.
    pub read_calls: u64,
}

/// A reader holds one file open across metadata discovery and JPEG extraction.
/// The generic reader permits in-memory fuzzing; normal operation uses File.
pub struct PreviewReader<R = File> {
    source: R,
    file_len: u64,
    stats: IoStats,
    metadata_bytes: u64,
    warnings: Vec<String>,
    primary_ifd: u64,
    orientation: u16,
}

#[derive(Clone, Copy)]
struct Ifd {
    offset: u64,
    sony: bool,
    end: u64,
}

impl PreviewReader<File> {
    pub fn open(path: &Path) -> Result<Self, Error> {
        let source = File::open(path)?;
        let metadata = source.metadata()?;
        if !metadata.is_file() {
            return Err(Error::Invalid("input is not a regular file"));
        }
        Ok(Self::from_reader(source, metadata.len()))
    }
}

pub fn find_best_preview(path: &Path) -> Result<EmbeddedPreview, Error> {
    PreviewReader::open(path)?.find_best_preview()
}

impl<R: Read + Seek> PreviewReader<R> {
    pub fn from_reader(source: R, file_len: u64) -> Self {
        Self {
            source,
            file_len,
            stats: IoStats::default(),
            metadata_bytes: 0,
            warnings: Vec::new(),
            primary_ifd: 0,
            orientation: 1,
        }
    }
    pub fn io_stats(&self) -> IoStats {
        self.stats
    }
    pub fn file_len(&self) -> u64 {
        self.file_len
    }
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// TIFF IFD0 orientation from the most recent preview discovery. No extra
    /// file access is performed; missing or invalid orientation defaults to 1.
    pub fn orientation(&self) -> u16 {
        self.orientation
    }

    fn read_at(&mut self, offset: u64, out: &mut [u8]) -> Result<(), Error> {
        checked_range(offset, out.len() as u64, self.file_len)?;
        self.stats.bytes_read = self.stats.bytes_read.saturating_add(out.len() as u64);
        self.stats.read_calls = self.stats.read_calls.saturating_add(1);
        self.source.seek(SeekFrom::Start(offset))?;
        self.source.read_exact(out)?;
        Ok(())
    }
    fn metadata(&mut self, offset: u64, out: &mut [u8]) -> Result<(), Error> {
        self.metadata_bytes = self
            .metadata_bytes
            .checked_add(out.len() as u64)
            .filter(|n| *n <= MAX_METADATA)
            .ok_or(Error::Limit("metadata read budget exceeded"))?;
        self.read_at(offset, out)
    }
    fn warn(&mut self, error: Error) {
        if self.warnings.len() < MAX_IFDS {
            self.warnings.push(error.to_string());
        }
    }

    pub fn find_best_preview(&mut self) -> Result<EmbeddedPreview, Error> {
        self.find_previews()?
            .into_iter()
            .next()
            .ok_or(Error::NoPreview)
    }

    /// Descending pixel area, then byte length. JPEG SOF is authoritative;
    /// raw-sensor ImageWidth/ImageLength tags are deliberately not used.
    pub fn find_previews(&mut self) -> Result<Vec<EmbeddedPreview>, Error> {
        self.metadata_bytes = 0;
        self.warnings.clear();
        self.primary_ifd = 0;
        self.orientation = 1;
        let mut header = [0; 8];
        self.metadata(0, &mut header)?;
        let endian = match &header[..2] {
            b"II" => Endian::Little,
            b"MM" => Endian::Big,
            _ => return Err(Error::Invalid("not a TIFF byte-order header")),
        };
        if endian.u16([header[2], header[3]]) != 42 {
            return Err(Error::Invalid(
                "requires classic TIFF magic 42; BigTIFF is unsupported",
            ));
        }
        let first = endian.u32([header[4], header[5], header[6], header[7]]) as u64;
        if first < 8 {
            return Err(Error::Invalid("invalid first IFD offset"));
        }
        self.primary_ifd = first;
        let mut queue = VecDeque::from([Ifd {
            offset: first,
            sony: false,
            end: self.file_len,
        }]);
        let mut visited = HashSet::new();
        let mut candidates = Vec::new();
        while let Some(ifd) = queue.pop_front() {
            if ifd.offset == 0 || !visited.insert((ifd.offset, ifd.sony)) {
                continue;
            }
            if visited.len() > MAX_IFDS {
                self.warn(Error::Limit("too many IFDs"));
                break;
            }
            if let Err(e) = self.walk_ifd(ifd, endian, &mut queue, &mut candidates) {
                self.warn(e);
            }
            if self.metadata_bytes >= MAX_METADATA {
                break;
            }
        }
        candidates.sort_unstable();
        candidates.dedup();
        let mut previews = Vec::new();
        for (offset, length) in candidates {
            match self.inspect_jpeg(offset, length) {
                Ok(preview) => previews.push(preview),
                Err(e) => self.warn(e),
            }
        }
        previews.sort_by_key(|p| {
            std::cmp::Reverse((
                u64::from(p.width.unwrap_or(0)) * u64::from(p.height.unwrap_or(0)),
                p.length,
            ))
        });
        if previews.is_empty() {
            return Err(Error::NoPreview);
        }
        Ok(previews)
    }

    fn pointers(&mut self, entry: Entry, endian: Endian) -> Result<Vec<u64>, Error> {
        if !matches!(entry.kind, 4 | 13) || entry.count == 0 {
            return Ok(Vec::new());
        }
        if entry.count as usize > MAX_POINTERS {
            return Err(Error::Limit("too many IFD pointers"));
        }
        if let Some(n) = entry.scalar(endian) {
            return Ok(vec![n]);
        }
        let mut values = vec![0; entry.count as usize * 4];
        self.metadata(endian.u32(entry.value) as u64, &mut values)?;
        Ok(values
            .chunks_exact(4)
            .map(|b| endian.u32([b[0], b[1], b[2], b[3]]) as u64)
            .collect())
    }

    fn walk_ifd(
        &mut self,
        ifd: Ifd,
        endian: Endian,
        queue: &mut VecDeque<Ifd>,
        candidates: &mut Vec<(u64, u64)>,
    ) -> Result<(), Error> {
        checked_range(ifd.offset, 2, ifd.end)?;
        let mut count = [0; 2];
        self.metadata(ifd.offset, &mut count)?;
        let count = endian.u16(count) as usize;
        if count > MAX_ENTRIES {
            return Err(Error::Limit("too many entries in IFD"));
        }
        let length = count
            .checked_mul(12)
            .and_then(|n| n.checked_add(4))
            .ok_or(Error::Invalid("IFD length overflow"))?;
        let entries_offset = ifd
            .offset
            .checked_add(2)
            .ok_or(Error::Invalid("IFD offset overflow"))?;
        // MakerNotes frequently end immediately after the entries: no next-IFD field.
        let entry_bytes = count * 12;
        checked_range(entries_offset, entry_bytes as u64, ifd.end)?;
        let has_next = checked_range(entries_offset, length as u64, ifd.end).is_ok();
        if !ifd.sony && !has_next {
            return Err(Error::Invalid("IFD next pointer truncated"));
        }
        let mut data = vec![0; if has_next { length } else { entry_bytes }];
        self.metadata(entries_offset, &mut data)?;
        let mut offset = None;
        let mut length = None;
        let mut strip_offset = None;
        let mut strip_length = None;
        let mut compression = None;
        let mut photometric = None;
        let mut bits = None;
        let mut samples = None;
        for chunk in data[..entry_bytes].chunks_exact(12) {
            let entry = Entry::parse(chunk, endian)?;
            match entry.tag {
                0x0201 => offset = entry.scalar(endian),
                0x0202 => length = entry.scalar(endian),
                0x0111 => strip_offset = entry.scalar(endian),
                0x0117 => strip_length = entry.scalar(endian),
                0x0103 => compression = entry.scalar(endian),
                0x0106 => photometric = entry.scalar(endian),
                0x0102 => bits = entry.scalar(endian),
                0x0115 => samples = entry.scalar(endian),
                0x0112 if !ifd.sony && ifd.offset == self.primary_ifd => {
                    if let Some(value @ 1..=8) = entry.scalar(endian) {
                        self.orientation = value as u16;
                    }
                }
                0x014a | 0x8769 | 0xa005 if !ifd.sony => match self.pointers(entry, endian) {
                    Ok(pointers) => {
                        for offset in pointers {
                            if queue.len() < MAX_IFDS {
                                queue.push_back(Ifd {
                                    offset,
                                    sony: false,
                                    end: self.file_len,
                                });
                            }
                        }
                    }
                    Err(e) => self.warn(e),
                },
                0x927c if !ifd.sony && matches!(entry.kind, 1 | 7) => {
                    if let Some(len) = entry.byte_len().filter(|n| *n >= 2) {
                        let start = endian.u32(entry.value) as u64;
                        if len > 4 && checked_range(start, len, self.file_len).is_ok() {
                            let mut signature = [0; 12];
                            let n = len.min(12) as usize;
                            if self.metadata(start, &mut signature[..n]).is_ok() {
                                let header_len = if signature.starts_with(b"SONY DSC \0\0\0")
                                    || signature.starts_with(b"SONY CAM \0\0\0")
                                {
                                    12
                                } else {
                                    0
                                };
                                if queue.len() < MAX_IFDS {
                                    queue.push_back(Ifd {
                                        offset: start + header_len,
                                        sony: true,
                                        end: start + len,
                                    });
                                }
                            }
                        }
                    }
                }
                0x2001 if ifd.sony && matches!(entry.kind, 1 | 7) => {
                    if let Some(len) = entry.byte_len().filter(|n| *n > 32) {
                        let start = endian.u32(entry.value) as u64;
                        // Sony PreviewImage: 32-byte proprietary prefix, TIFF-relative pointer.
                        if checked_range(start, len, self.file_len).is_ok() {
                            self.add_candidate(candidates, start + 32, len - 32);
                        }
                    }
                }
                _ => {} // Unknown and potentially enormous entries are never materialized.
            }
        }
        if let (Some(offset), Some(length)) = (offset, length) {
            self.add_candidate(candidates, offset, length);
        }
        // ARW lossless RAW data also uses compression 7. Exclude sensor/CFA
        // strips before inspecting any payload, even if they resemble JPEG.
        if matches!(compression, Some(6 | 7))
            && matches!(photometric, Some(1 | 2 | 6))
            && !matches!(bits, Some(n) if n != 8)
            && !matches!(samples, Some(n) if !matches!(n, 1 | 3 | 4))
        {
            if let (Some(offset), Some(length)) = (strip_offset, strip_length) {
                self.add_candidate(candidates, offset, length);
            }
        }
        if !ifd.sony && has_next {
            let b = &data[entry_bytes..];
            let next = endian.u32([b[0], b[1], b[2], b[3]]) as u64;
            if next != 0 && queue.len() < MAX_IFDS {
                queue.push_back(Ifd {
                    offset: next,
                    sony: false,
                    end: self.file_len,
                });
            }
        }
        Ok(())
    }

    fn add_candidate(&mut self, candidates: &mut Vec<(u64, u64)>, offset: u64, length: u64) {
        if candidates.len() >= MAX_CANDIDATES {
            return;
        }
        if !(4..=MAX_JPEG_BYTES).contains(&length)
            || checked_range(offset, length, self.file_len).is_err()
        {
            self.warn(Error::Invalid("invalid embedded preview range"));
        } else {
            candidates.push((offset, length));
        }
    }

    fn inspect_jpeg(&mut self, offset: u64, length: u64) -> Result<EmbeddedPreview, Error> {
        // Small window; large APP payloads are skipped by their declared segment
        // length, not loaded. Each candidate gets at most 1 MiB of header travel.
        let mut window = [0; 4096];
        let mut window_start = u64::MAX;
        let mut window_len = 0;
        let mut byte = |this: &mut Self, pos: u64| -> Result<u8, Error> {
            if pos >= length || pos >= jpeg::MAX_HEADER {
                return Err(Error::Invalid(
                    "JPEG header exceeds preview or header limit",
                ));
            }
            if window_start == u64::MAX
                || pos < window_start
                || pos >= window_start + window_len as u64
            {
                window_start = pos;
                window_len = (length - pos).min(window.len() as u64) as usize;
                this.metadata(offset + pos, &mut window[..window_len])?;
            }
            Ok(window[(pos - window_start) as usize])
        };
        if byte(self, 0)? != 0xff || byte(self, 1)? != 0xd8 {
            return Err(Error::Invalid("candidate lacks JPEG SOI"));
        }
        let mut pos = 2u64;
        let (width, height) = loop {
            if byte(self, pos)? != 0xff {
                return Err(Error::Invalid("JPEG header marker missing"));
            }
            while byte(self, pos)? == 0xff {
                pos += 1;
            }
            let marker = byte(self, pos)?;
            pos += 1;
            if marker == 0x01 {
                continue;
            }
            if matches!(marker, 0 | 0xd0..=0xda) {
                return Err(Error::Invalid("JPEG has no supported SOF before scan"));
            }
            let segment_len = u16::from_be_bytes([byte(self, pos)?, byte(self, pos + 1)?]) as u64;
            if segment_len < 2 {
                return Err(Error::Invalid("short JPEG segment length"));
            }
            let end = pos
                .checked_add(segment_len)
                .filter(|n| *n <= length)
                .ok_or(Error::Invalid("JPEG segment outside candidate"))?;
            if jpeg::is_sof(marker) {
                if !matches!(marker, 0xc0..=0xc2) {
                    return Err(Error::Invalid(
                        "JPEG is not baseline/extended/progressive DCT",
                    ));
                }
                let payload_len = segment_len - 2;
                if !(9..=18).contains(&payload_len) {
                    return Err(Error::Invalid("invalid JPEG SOF length"));
                }
                let mut payload = [0; 18];
                for i in 0..payload_len {
                    payload[i as usize] = byte(self, pos + 2 + i)?;
                }
                break jpeg::dimensions(&payload[..payload_len as usize])?;
            }
            pos = end;
        };
        // Allow only bounded zero/FF padding beyond the declared JPEG end.
        let n = length.min(4096) as usize;
        let mut tail = [0; 4096];
        self.metadata(offset + length - n as u64, &mut tail[..n])?;
        let eoi = (1..n)
            .rev()
            .find(|i| tail[*i - 1] == 0xff && tail[*i] == 0xd9)
            .ok_or(Error::Invalid("candidate lacks terminal JPEG EOI"))?;
        if tail[eoi + 1..n].iter().any(|b| *b != 0 && *b != 0xff) {
            return Err(Error::Invalid("unexpected data following JPEG EOI"));
        }
        let length = length - n as u64 + eoi as u64 + 1;
        Ok(EmbeddedPreview {
            offset,
            length,
            width: Some(width),
            height: Some(height),
        })
    }

    /// Reads only the selected JPEG. The caller can retain this buffer across files.
    /// Validates marker framing and trims zero/FF padding; does not entropy-decode.
    pub fn read_preview_into(
        &mut self,
        preview: &EmbeddedPreview,
        out: &mut Vec<u8>,
    ) -> Result<(), Error> {
        let result = (|| {
            if !(4..=MAX_JPEG_BYTES).contains(&preview.length) {
                return Err(Error::Limit("JPEG byte length"));
            }
            checked_range(preview.offset, preview.length, self.file_len)?;
            let n = usize::try_from(preview.length)
                .map_err(|_| Error::Limit("JPEG does not fit address space"))?;
            if n > out.len() {
                out.try_reserve_exact(n - out.len())
                    .map_err(|_| Error::Limit("JPEG allocation failed"))?;
            }
            // Preserve initialized storage: repeated reads do not zero the entire
            // previous JPEG buffer before immediately overwriting it from disk.
            out.resize(n, 0);
            self.read_at(preview.offset, out)?;
            jpeg::validate(out)
        })();
        match result {
            Ok(end) => {
                out.truncate(end);
                Ok(())
            }
            Err(e) => {
                out.clear();
                Err(e)
            }
        }
    }
}

#[cfg(test)]
mod orientation_tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn discovery_reuses_primary_ifd_orientation_in_both_endians() {
        for big in [false, true] {
            for value in 0..=9u16 {
                let u16_bytes = |n: u16| {
                    if big {
                        n.to_be_bytes()
                    } else {
                        n.to_le_bytes()
                    }
                };
                let u32_bytes = |n: u32| {
                    if big {
                        n.to_be_bytes()
                    } else {
                        n.to_le_bytes()
                    }
                };
                let mut bytes = vec![0; 64];
                bytes[..2].copy_from_slice(if big { b"MM" } else { b"II" });
                bytes[2..4].copy_from_slice(&u16_bytes(42));
                bytes[4..8].copy_from_slice(&u32_bytes(8));
                for (offset, orientation, next) in [(8, value, 32), (32, 3, 0)] {
                    bytes[offset..offset + 2].copy_from_slice(&u16_bytes(1));
                    bytes[offset + 2..offset + 4].copy_from_slice(&u16_bytes(0x0112));
                    bytes[offset + 4..offset + 6].copy_from_slice(&u16_bytes(3));
                    bytes[offset + 6..offset + 10].copy_from_slice(&u32_bytes(1));
                    bytes[offset + 10..offset + 12].copy_from_slice(&u16_bytes(orientation));
                    bytes[offset + 14..offset + 18].copy_from_slice(&u32_bytes(next));
                }
                let mut reader = PreviewReader::from_reader(Cursor::new(bytes), 64);
                assert!(matches!(reader.find_previews(), Err(Error::NoPreview)));
                let before = reader.io_stats().bytes_read;
                assert_eq!(
                    reader.orientation(),
                    if (1..=8).contains(&value) { value } else { 1 }
                );
                assert_eq!(
                    reader.io_stats().bytes_read,
                    before,
                    "getter performed extra I/O"
                );
            }
        }
    }
}
