//! Reusable RGBA output and bounded JPEG decoding. No RAW decoding takes place here.

/// Limits are intentionally independent of the dimensions advertised by an ARW IFD.
pub const MAX_DIMENSION: usize = 16_384;
pub const MAX_DECODED_BYTES: usize = 256 * 1024 * 1024;
pub const MAX_JPEG_BYTES: usize = 128 * 1024 * 1024;
const MAX_HEADER_BYTES: usize = 4 * 1024 * 1024;
const MAX_HEADER_MARKERS: usize = 1024;
#[cfg(any(feature = "turbo", feature = "zune"))]
const MAX_PROGRESSIVE_SCANS: usize = 100;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Backend {
    Turbo,
    Zune,
}

impl Backend {
    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "turbo" | "turbojpeg" => Ok(Self::Turbo),
            "zune" => Ok(Self::Zune),
            _ => Err(format!("unknown JPEG backend {value:?}; use turbo or zune")),
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::Turbo => "turbo",
            Self::Zune => "zune",
        }
    }

    pub fn available() -> &'static [Self] {
        &[
            #[cfg(feature = "turbo")]
            Self::Turbo,
            #[cfg(feature = "zune")]
            Self::Zune,
        ]
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DecodedInfo {
    pub width: u32,
    pub height: u32,
    /// Bytes of tightly packed, top-to-bottom RGBA8 pixels.
    pub bytes: usize,
}

/// Keep one decoder on each worker. The output allocation is reused across files.
/// Pixel data remains valid until the next `decode` call.
pub struct Decoder {
    backend: Backend,
    pixels: Vec<u8>,
    valid_bytes: usize,
    #[cfg(feature = "turbo")]
    turbo: Option<turbojpeg::Decompressor>,
    #[cfg(feature = "turbo")]
    scaling_denominator: u32,
}

impl Decoder {
    pub fn new(backend: Backend) -> Result<Self, String> {
        Self::with_scale(backend, 1)
    }

    /// Decode at 1/denominator size using TurboJPEG's reduced IDCT output.
    /// Original JPEG bounds still apply; this never allocates full-size pixels
    /// merely to resize them afterward. Zune supports only full-size output here.
    pub fn with_scale(backend: Backend, denominator: u32) -> Result<Self, String> {
        if !matches!(denominator, 1 | 2 | 4 | 8) {
            return Err("JPEG scale denominator must be 1, 2, 4, or 8".into());
        }
        if backend == Backend::Zune && denominator != 1 {
            return Err("zune backend does not support native reduced-size JPEG decoding".into());
        }
        if !Backend::available().contains(&backend) {
            return Err(format!(
                "JPEG backend {} is disabled; rebuild with --features {}",
                backend.name(),
                backend.name()
            ));
        }
        #[cfg(feature = "turbo")]
        let turbo = if backend == Backend::Turbo {
            Some(Self::new_turbo(denominator)?)
        } else {
            None
        };
        Ok(Self {
            backend,
            pixels: Vec::new(),
            valid_bytes: 0,
            #[cfg(feature = "turbo")]
            turbo,
            #[cfg(feature = "turbo")]
            scaling_denominator: denominator,
        })
    }

    #[cfg(feature = "turbo")]
    fn new_turbo(denominator: u32) -> Result<turbojpeg::Decompressor, String> {
        let mut decoder = turbojpeg::Decompressor::new().map_err(|e| e.to_string())?;
        decoder
            .set_scan_limit(MAX_PROGRESSIVE_SCANS as u32)
            .map_err(|e| e.to_string())?;
        // Match zune's triangle/fancy chroma filtering. Its `new_fast`
        // enables SIMD; it does not select reduced-quality interpolation.
        decoder
            .set_fast_upsample(false)
            .map_err(|e| e.to_string())?;
        // Callers have already restricted this to native 1, 2, 4, 8 ratios.
        decoder
            .set_scaling_factor(turbojpeg::ScalingFactor::new(1, denominator as usize))
            .map_err(|e| e.to_string())?;
        Ok(decoder)
    }

    pub fn backend(&self) -> Backend {
        self.backend
    }

    pub fn pixels(&self) -> &[u8] {
        &self.pixels[..self.valid_bytes]
    }

    /// Transfer the completed RGBA allocation to a background upload/cache owner.
    /// A failed or absent decode exposes no stale pixels. The next decode remains
    /// valid and obtains a fresh buffer as needed.
    pub fn take_pixels(&mut self) -> Vec<u8> {
        let valid = std::mem::take(&mut self.valid_bytes);
        let mut pixels = std::mem::take(&mut self.pixels);
        pixels.truncate(valid);
        pixels
    }

    /// The allocation is retained after smaller images, avoiding repeated zero fills.
    /// Failed decodes expose no pixels, including partially written output.
    pub fn decode(&mut self, jpeg: &[u8]) -> Result<DecodedInfo, String> {
        self.valid_bytes = 0;
        let original = jpeg_layout(jpeg)?;
        let info = match self.backend {
            Backend::Turbo => match self.decode_turbo(jpeg, original) {
                Ok(info) => info,
                Err(error) => {
                    // A malformed native header can leave libjpeg's input
                    // parser mid-frame. Recreate only on failure so the next
                    // photograph cannot inherit that state; keep output reuse.
                    #[cfg(feature = "turbo")]
                    {
                        self.turbo = None;
                        self.turbo =
                            Some(Self::new_turbo(self.scaling_denominator).map_err(|reset| {
                                format!("{error}; unable to reset TurboJPEG: {reset}")
                            })?);
                    }
                    return Err(error);
                }
            },
            Backend::Zune => {
                self.decode_zune(jpeg, original)?;
                original
            }
        };
        self.valid_bytes = info.bytes;
        Ok(info)
    }

    #[cfg(feature = "turbo")]
    fn decode_turbo(&mut self, jpeg: &[u8], original: DecodedInfo) -> Result<DecodedInfo, String> {
        let decoder = self.turbo.as_mut().ok_or("TurboJPEG is not initialized")?;
        let header = decoder.read_header(jpeg).map_err(|e| e.to_string())?;
        if header.width != original.width as usize || header.height != original.height as usize {
            return Err("JPEG dimensions changed between header readers".into());
        }
        // The verified source dimensions are bounded before the crate's ceil
        // scaling arithmetic. Only the scaled RGBA output is materialized.
        let scaled = header.scaled(decoder.scaling_factor());
        let info = checked_layout(scaled.width, scaled.height)?;
        prepare_buffer(&mut self.pixels, info.bytes)?;
        decoder
            .decompress(
                jpeg,
                turbojpeg::Image {
                    pixels: &mut self.pixels[..info.bytes],
                    width: scaled.width,
                    pitch: scaled.width * 4, // checked in checked_layout
                    height: scaled.height,
                    format: turbojpeg::PixelFormat::RGBA,
                },
            )
            .map_err(|e| e.to_string())?;
        Ok(info)
    }

    #[cfg(not(feature = "turbo"))]
    fn decode_turbo(&mut self, _: &[u8], _: DecodedInfo) -> Result<DecodedInfo, String> {
        Err("TurboJPEG backend is disabled".into())
    }

    #[cfg(feature = "zune")]
    fn decode_zune(&mut self, jpeg: &[u8], info: DecodedInfo) -> Result<(), String> {
        use std::panic::{catch_unwind, AssertUnwindSafe};
        use zune_core::{bytestream::ZCursor, colorspace::ColorSpace, options::DecoderOptions};

        // The crate documents malformed-input panics as possible. Its per-file state
        // is discarded on unwind, and partial output is never made visible.
        catch_unwind(AssertUnwindSafe(|| {
            let options = DecoderOptions::new_fast()
                .set_strict_mode(true)
                .set_max_width(MAX_DIMENSION)
                .set_max_height(MAX_DIMENSION)
                .jpeg_set_max_scans(MAX_PROGRESSIVE_SCANS)
                .jpeg_set_out_colorspace(ColorSpace::RGBA);
            let mut decoder = zune_jpeg::JpegDecoder::new_with_options(ZCursor::new(jpeg), options);
            decoder.decode_headers().map_err(|e| e.to_string())?;
            if decoder.dimensions() != Some((info.width as usize, info.height as usize))
                || decoder.output_buffer_size() != Some(info.bytes)
                || decoder.output_colorspace() != Some(ColorSpace::RGBA)
            {
                return Err("JPEG decoder cannot produce the bounded RGBA layout".into());
            }
            prepare_buffer(&mut self.pixels, info.bytes)?;
            decoder
                .decode_into(&mut self.pixels[..info.bytes])
                .map_err(|e| e.to_string())
        }))
        .map_err(|_| "JPEG decoder rejected malformed input with an internal panic".to_string())?
    }

    #[cfg(not(feature = "zune"))]
    fn decode_zune(&mut self, _: &[u8], _: DecodedInfo) -> Result<(), String> {
        Err("zune-jpeg backend is disabled".into())
    }
}

#[cfg(any(feature = "turbo", feature = "zune"))]
fn prepare_buffer(pixels: &mut Vec<u8>, bytes: usize) -> Result<(), String> {
    if pixels.len() < bytes {
        pixels
            .try_reserve_exact(bytes - pixels.len())
            .map_err(|e| format!("unable to reserve JPEG output buffer: {e}"))?;
        pixels.resize(bytes, 0);
    }
    Ok(())
}

fn checked_layout(width: usize, height: usize) -> Result<DecodedInfo, String> {
    if width == 0 || height == 0 || width > MAX_DIMENSION || height > MAX_DIMENSION {
        return Err(format!(
            "JPEG dimensions {width}x{height} exceed supported limits"
        ));
    }
    let bytes = width
        .checked_mul(height)
        .and_then(|pixels| pixels.checked_mul(4))
        .filter(|&bytes| bytes <= MAX_DECODED_BYTES)
        .ok_or("JPEG RGBA output exceeds the 256 MiB limit")?;
    Ok(DecodedInfo {
        width: width as u32,
        height: height as u32,
        bytes,
    })
}

/// Check allocation-critical JPEG fields before invoking either decoder's parser.
/// Marker lengths skip APP payloads; entropy data is never scanned here.
pub(crate) fn jpeg_layout(jpeg: &[u8]) -> Result<DecodedInfo, String> {
    if jpeg.len() > MAX_JPEG_BYTES {
        return Err("compressed JPEG exceeds the 128 MiB limit".into());
    }
    if !jpeg.starts_with(&[0xff, 0xd8]) {
        return Err("missing JPEG start marker".into());
    }
    let mut offset = 2usize;
    let mut layout = None;
    for _ in 0..MAX_HEADER_MARKERS {
        if offset >= MAX_HEADER_BYTES || jpeg.get(offset) != Some(&0xff) {
            return Err("invalid or oversized JPEG header".into());
        }
        while jpeg.get(offset) == Some(&0xff) {
            offset += 1;
            if offset >= MAX_HEADER_BYTES {
                return Err("oversized JPEG header".into());
            }
        }
        let marker = *jpeg.get(offset).ok_or("truncated JPEG marker")?;
        offset += 1;
        if matches!(marker, 0x00 | 0x01 | 0xd0..=0xd9) {
            return Err("unexpected standalone JPEG marker in header".into());
        }
        let length_bytes = jpeg
            .get(offset..offset + 2)
            .ok_or("truncated JPEG length")?;
        let length = u16::from_be_bytes([length_bytes[0], length_bytes[1]]) as usize;
        let end = offset
            .checked_add(length)
            .ok_or("JPEG header offset overflow")?;
        if length < 2 || end > MAX_HEADER_BYTES {
            return Err("invalid or oversized JPEG segment".into());
        }
        let segment = jpeg.get(offset + 2..end).ok_or("truncated JPEG segment")?;
        // SOF marker codes exclude DHT (C4), JPG (C8), and DAC (CC).
        if matches!(marker, 0xc0..=0xc3 | 0xc5..=0xc7 | 0xc9..=0xcb | 0xcd..=0xcf) {
            if !matches!(marker, 0xc0..=0xc2) || layout.is_some() {
                return Err("unsupported JPEG coding process or multiple frames".into());
            }
            if segment.len() < 6 || segment[0] != 8 {
                return Err("only 8-bit embedded JPEG previews are supported".into());
            }
            let components = segment[5] as usize;
            if !matches!(components, 1 | 3) || segment.len() != 6 + 3 * components {
                return Err("unsupported JPEG component layout".into());
            }
            for component in segment[6..].chunks_exact(3) {
                let sampling = component[1];
                if !(1..=4).contains(&(sampling >> 4)) || !(1..=4).contains(&(sampling & 15)) {
                    return Err("invalid JPEG component sampling".into());
                }
            }
            layout = Some(checked_layout(
                u16::from_be_bytes([segment[3], segment[4]]) as usize,
                u16::from_be_bytes([segment[1], segment[2]]) as usize,
            )?);
        }
        if marker == 0xda {
            return layout.ok_or("JPEG scan precedes its frame header".into());
        }
        offset = end;
    }
    Err("JPEG header marker limit exceeded".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jpeg_header(width: u16, height: u16) -> Vec<u8> {
        let mut data = vec![0xff, 0xd8, 0xff, 0xc0, 0, 11, 8];
        data.extend_from_slice(&height.to_be_bytes());
        data.extend_from_slice(&width.to_be_bytes());
        data.extend_from_slice(&[1, 1, 0x11, 0, 0xff, 0xda, 0, 8, 1, 1, 0, 0, 63, 0]);
        data
    }

    #[test]
    fn validates_allocation_before_decode() {
        let info = jpeg_layout(&jpeg_header(7008, 4672)).unwrap();
        assert_eq!(
            (info.width, info.height, info.bytes),
            (7008, 4672, 130_965_504)
        );
        assert!(jpeg_layout(&jpeg_header(16_384, 16_384)).is_err());
        assert!(jpeg_layout(&jpeg_header(0, 100)).is_err());
        assert!(checked_layout(usize::MAX, usize::MAX).is_err());
    }

    #[test]
    fn rejects_every_truncated_header() {
        let jpeg = jpeg_header(32, 16);
        for length in 0..jpeg.len() {
            assert!(jpeg_layout(&jpeg[..length]).is_err(), "length {length}");
        }
        assert!(jpeg_layout(&jpeg).is_ok());
    }

    #[test]
    fn rejects_oversized_dimensions_and_segments() {
        assert!(jpeg_layout(&jpeg_header(65_535, 1)).is_err());
        assert!(jpeg_layout(&[0xff, 0xd8, 0xff, 0xe1, 0, 1]).is_err());
        assert!(jpeg_layout(&[0xff, 0xd8, 0xff, 0xe1, 0xff, 0xff]).is_err());
    }

    #[test]
    fn available_backends_reject_corrupt_input_without_exposing_pixels() {
        for &backend in Backend::available() {
            let mut decoder = Decoder::new(backend).unwrap();
            assert!(decoder.decode(&jpeg_header(32, 16)).is_err());
            assert!(decoder.pixels().is_empty());
        }
    }

    /// A self-contained 8x8 gray JPEG: constant quantizer, DC zero, AC EOB.
    /// This generated fixture contains no third-party photographic content.
    fn neutral_jpeg() -> Vec<u8> {
        let mut data = vec![0xff, 0xd8, 0xff, 0xdb, 0, 67, 0];
        data.extend_from_slice(&[1; 64]);
        data.extend_from_slice(&[0xff, 0xc0, 0, 11, 8, 0, 8, 0, 8, 1, 1, 0x11, 0]);
        data.extend_from_slice(&[0xff, 0xc4, 0, 38]);
        for table in [0, 0x10] {
            data.extend_from_slice(&[table, 1]);
            data.extend_from_slice(&[0; 15]);
            data.push(0);
        }
        data.extend_from_slice(&[0xff, 0xda, 0, 8, 1, 1, 0, 0, 63, 0, 0x3f, 0xff, 0xd9]);
        data
    }

    #[test]
    fn decodes_neutral_jpeg_and_reuses_output() {
        let jpeg = neutral_jpeg();
        for &backend in Backend::available() {
            let mut decoder = Decoder::new(backend).unwrap();
            let info = decoder.decode(&jpeg).unwrap();
            assert_eq!((info.width, info.height, info.bytes), (8, 8, 256));
            assert!(decoder
                .pixels()
                .chunks_exact(4)
                .all(|p| p == [128, 128, 128, 255]));
            let allocation = decoder.pixels.as_ptr();
            decoder.decode(&jpeg).unwrap();
            assert_eq!(decoder.pixels.as_ptr(), allocation);
            assert!(decoder.decode(&[]).is_err());
            assert!(decoder.pixels().is_empty());
            decoder.decode(&jpeg).unwrap();
            assert_eq!(decoder.pixels.as_ptr(), allocation);
        }
    }

    #[cfg(feature = "turbo")]
    #[test]
    fn turbo_recovers_after_native_header_failure_at_every_scale() {
        let jpeg = neutral_jpeg();
        let mut malformed = jpeg.clone();
        let sos = malformed
            .windows(2)
            .position(|p| p == [0xff, 0xda])
            .unwrap();
        malformed[sos + 5] = 2; // Unknown component ID passes framing validation.
        for denominator in [1, 2, 4, 8] {
            let mut decoder = Decoder::with_scale(Backend::Turbo, denominator).unwrap();
            let expected = decoder.decode(&jpeg).unwrap();
            let allocation = decoder.pixels.as_ptr();
            assert!(decoder.decode(&malformed).is_err());
            assert!(decoder.pixels().is_empty());
            assert_eq!(decoder.decode(&jpeg).unwrap(), expected);
            assert_eq!(decoder.pixels.as_ptr(), allocation);
            assert!(decoder
                .pixels()
                .chunks_exact(4)
                .all(|p| p == [128, 128, 128, 255]));
        }
    }

    #[test]
    fn transfers_pixels_without_copy_and_can_decode_again() {
        let jpeg = neutral_jpeg();
        for &backend in Backend::available() {
            let mut decoder = Decoder::new(backend).unwrap();
            decoder.decode(&jpeg).unwrap();
            let allocation = decoder.pixels().as_ptr();
            let owned = decoder.take_pixels();
            assert_eq!(owned.as_ptr(), allocation);
            assert_eq!(owned.len(), 256);
            assert!(decoder.pixels().is_empty());
            assert!(decoder.take_pixels().is_empty());
            decoder.decode(&jpeg).unwrap();
            assert_eq!(decoder.pixels(), owned);
            assert!(decoder.decode(&[]).is_err());
            assert!(decoder.take_pixels().is_empty());
        }
    }

    #[test]
    fn rejects_invalid_scales_and_zune_reduction() {
        for backend in [Backend::Turbo, Backend::Zune] {
            for denominator in [0, 3, 5, 7, 16, u32::MAX] {
                assert!(Decoder::with_scale(backend, denominator).is_err());
            }
        }
        for denominator in [2, 4, 8] {
            assert!(Decoder::with_scale(Backend::Zune, denominator).is_err());
        }
    }

    #[cfg(feature = "turbo")]
    #[test]
    fn turbo_reduced_decode_allocates_only_scaled_pixels_and_reuses_output() {
        let jpeg = neutral_jpeg();
        for denominator in [2, 4, 8] {
            let mut decoder = Decoder::with_scale(Backend::Turbo, denominator).unwrap();
            let info = decoder.decode(&jpeg).unwrap();
            let side = 8 / denominator;
            let bytes = (side * side * 4) as usize;
            assert_eq!((info.width, info.height, info.bytes), (side, side, bytes));
            assert_eq!(decoder.pixels.len(), bytes);
            assert!(decoder.pixels.capacity() < 8 * 8 * 4);
            assert!(decoder
                .pixels()
                .chunks_exact(4)
                .all(|p| p == [128, 128, 128, 255]));
            let allocation = decoder.pixels.as_ptr();
            decoder.decode(&jpeg).unwrap();
            assert_eq!(decoder.pixels.as_ptr(), allocation);
        }
    }

    #[cfg(feature = "turbo")]
    #[test]
    fn turbo_reduced_decode_rounds_dimensions_up_and_keeps_source_bounds() {
        let mut jpeg = neutral_jpeg();
        // A 7x5 crop still occupies the fixture's single 8x8 encoded MCU.
        jpeg[76..78].copy_from_slice(&5u16.to_be_bytes());
        jpeg[78..80].copy_from_slice(&7u16.to_be_bytes());
        for denominator in [2, 4, 8] {
            let mut decoder = Decoder::with_scale(Backend::Turbo, denominator).unwrap();
            let info = decoder.decode(&jpeg).unwrap();
            assert_eq!(
                (info.width, info.height),
                (7u32.div_ceil(denominator), 5u32.div_ceil(denominator))
            );
            assert!(decoder.decode(&jpeg_header(16_384, 16_384)).is_err());
            assert!(decoder.pixels().is_empty());
        }
    }
}
