//! JPEG framing only: never reads or decodes a RAW strip. Entropy validation is
//! the decoder's job; this checks marker lengths and the true EOI boundary.
use super::Error;

pub(super) const MAX_HEADER: u64 = 1024 * 1024;

pub(super) fn is_sof(marker: u8) -> bool {
    matches!(marker, 0xc0..=0xc3 | 0xc5..=0xc7 | 0xc9..=0xcb | 0xcd..=0xcf)
}

pub(super) fn dimensions(data: &[u8]) -> Result<(u32, u32), Error> {
    if data.len() < 6 || data[0] != 8 {
        return Err(Error::Invalid("unsupported JPEG precision or short SOF"));
    }
    let height = u16::from_be_bytes([data[1], data[2]]) as u32;
    let width = u16::from_be_bytes([data[3], data[4]]) as u32;
    let components = usize::from(data[5]);
    if width == 0
        || height == 0
        || !matches!(components, 1 | 3 | 4)
        || data.len() != 6 + components * 3
    {
        return Err(Error::Invalid("invalid JPEG SOF dimensions or components"));
    }
    Ok((width, height))
}

pub(super) fn validate(data: &[u8]) -> Result<usize, Error> {
    if !data.starts_with(&[0xff, 0xd8]) {
        return Err(Error::Invalid("JPEG SOI missing"));
    }
    let mut pos = 2usize;
    let mut frame = false;
    let mut scan = false;
    let mut entropy = false;
    while pos < data.len() {
        if entropy {
            while pos < data.len() && data[pos] != 0xff {
                pos += 1;
            }
        }
        if data.get(pos) != Some(&0xff) {
            return Err(Error::Invalid("JPEG marker missing"));
        }
        while data.get(pos) == Some(&0xff) {
            pos += 1;
        }
        let marker = *data
            .get(pos)
            .ok_or(Error::Invalid("truncated JPEG marker"))?;
        pos += 1;
        if entropy && (marker == 0 || matches!(marker, 0xd0..=0xd7)) {
            continue;
        }
        entropy = false;
        if marker == 0xd9 {
            if !frame || !scan {
                return Err(Error::Invalid("JPEG lacks frame or scan"));
            }
            if data[pos..].iter().any(|b| *b != 0 && *b != 0xff) {
                return Err(Error::Invalid("non-padding bytes after JPEG EOI"));
            }
            return Ok(pos);
        }
        if marker == 0x01 {
            continue;
        }
        if matches!(marker, 0 | 0xd0..=0xd8) {
            return Err(Error::Invalid("unexpected standalone JPEG marker"));
        }
        let length_bytes = data
            .get(pos..pos.checked_add(2).ok_or(Error::Invalid("JPEG overflow"))?)
            .ok_or(Error::Invalid("short JPEG segment"))?;
        let length = usize::from(u16::from_be_bytes([length_bytes[0], length_bytes[1]]));
        if length < 2 {
            return Err(Error::Invalid("invalid JPEG segment length"));
        }
        let end = pos
            .checked_add(length)
            .filter(|end| *end <= data.len())
            .ok_or(Error::Invalid("JPEG segment outside preview"))?;
        if is_sof(marker) {
            if !matches!(marker, 0xc0..=0xc2) {
                return Err(Error::Invalid("unsupported JPEG frame type"));
            }
            dimensions(&data[pos + 2..end])?;
            frame = true;
        }
        if marker == 0xda {
            let payload = &data[pos + 2..end];
            let n = payload.first().copied().unwrap_or(0) as usize;
            if !frame || n == 0 || n > 4 || payload.len() != 4 + n * 2 {
                return Err(Error::Invalid("invalid JPEG scan header"));
            }
            scan = true;
            entropy = true;
        }
        pos = end;
    }
    Err(Error::Invalid("JPEG EOI missing"))
}

#[cfg(test)]
pub(super) fn fixture(width: u16, height: u16) -> Vec<u8> {
    let mut jpeg = vec![0xff, 0xd8, 0xff, 0xc0, 0, 11, 8];
    jpeg.extend_from_slice(&height.to_be_bytes());
    jpeg.extend_from_slice(&width.to_be_bytes());
    jpeg.extend_from_slice(&[
        1, 1, 0x11, 0, 0xff, 0xda, 0, 8, 1, 1, 0, 0, 63, 0, 0x23, 0xff, 0, 0x45, 0xff, 0xd0, 0x67,
        0xff, 0xd9,
    ]);
    jpeg
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn boundaries_stuffing_restarts_and_padding() {
        let mut jpeg = fixture(6000, 4000);
        let len = jpeg.len();
        assert_eq!(validate(&jpeg).unwrap(), len);
        jpeg.extend_from_slice(&[0, 0xff, 0]);
        assert_eq!(validate(&jpeg).unwrap(), len);
        jpeg.push(42);
        assert!(validate(&jpeg).is_err());
    }
    #[test]
    fn rejects_every_truncation() {
        let jpeg = fixture(12, 8);
        for n in 0..jpeg.len() {
            assert!(validate(&jpeg[..n]).is_err(), "length {n}");
        }
    }
    #[test]
    fn skips_fake_end_marker_inside_app_segment() {
        let jpeg = fixture(12, 8);
        let mut patched = vec![0xff, 0xd8, 0xff, 0xe1, 0, 4, 0xff, 0xd9];
        patched.extend_from_slice(&jpeg[2..]);
        assert_eq!(validate(&patched).unwrap(), patched.len());
        patched[4..6].copy_from_slice(&1u16.to_be_bytes());
        assert!(validate(&patched).is_err());
    }
}
