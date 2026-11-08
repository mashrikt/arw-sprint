//! One open handle for a JPEG photograph or the embedded previews of ARW/DNG.
//! Discovery stays separate from the budgeted image-byte allocation.
use crate::{
    arw::{EmbeddedPreview, Error, IoStats, PreviewReader},
    browser::directory::is_jpeg,
    metadata::read_jpeg_header,
};
use std::{fs::File, io::Cursor, path::Path};

pub struct PreviewSource {
    reader: PreviewReader,
    jpeg: bool,
    allow_large_thumbnail: bool,
    orientation: u16,
    thumbnails: Vec<EmbeddedPreview>,
    icc_profile: Option<Vec<u8>>,
    adobe_rgb: bool,
}

impl PreviewSource {
    pub fn open(path: &Path) -> Result<Self, Error> {
        let mut file = File::open(path)?;
        let stat = file.metadata()?;
        if !stat.is_file() {
            return Err(Error::Invalid("input is not a regular file"));
        }
        let jpeg = is_jpeg(path);
        let allow_large_thumbnail = jpeg
            || path
                .extension()
                .is_some_and(|extension| extension.as_encoded_bytes().eq_ignore_ascii_case(b"dng"));
        let mut orientation = 1;
        let mut thumbnails = Vec::new();
        let mut icc_profile = None;
        let mut adobe_rgb = false;
        if jpeg {
            if stat.len() > super::decoder::MAX_JPEG_BYTES as u64 {
                return Err(Error::Limit("JPEG byte length"));
            }
            let header = read_jpeg_header(&mut file, stat.len()).map_err(|error| {
                Error::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, error))
            })?;
            orientation = header.exif.orientation;
            adobe_rgb = header.exif.adobe_rgb;
            icc_profile = header.icc_profile;
            if let Some((base, tiff)) = header.exif_tiff {
                let mut exif = PreviewReader::from_reader(Cursor::new(&tiff), tiff.len() as u64);
                if let Ok(previews) = exif.find_previews() {
                    for mut preview in previews {
                        preview.offset = base
                            .checked_add(preview.offset)
                            .ok_or(Error::Invalid("EXIF thumbnail offset overflow"))?;
                        thumbnails.push(preview);
                    }
                }
            }
        }
        Ok(Self {
            reader: PreviewReader::from_reader(file, stat.len()),
            jpeg,
            allow_large_thumbnail,
            orientation,
            thumbnails,
            icc_profile,
            adobe_rgb,
        })
    }

    pub fn find_previews(&mut self) -> Result<Vec<EmbeddedPreview>, Error> {
        if self.jpeg {
            let full = self.reader.inspect_jpeg(0, self.reader.file_len())?;
            // Main-image callers must not silently fall back to an EXIF
            // thumbnail if the standalone photograph is corrupt/unsupported.
            Ok(vec![full])
        } else {
            let previews = self.reader.find_previews();
            self.orientation = self.reader.orientation();
            previews
        }
    }
    /// Filmstrip-only candidates. A JPEG's APP1 thumbnails are intentionally
    /// excluded from main-image discovery, even when its entropy decode fails.
    pub fn find_thumbnail_previews(&mut self) -> Result<Vec<EmbeddedPreview>, Error> {
        let mut previews = self.find_previews()?;
        if self.jpeg {
            previews.extend(self.thumbnails.iter().cloned());
        }
        Ok(previews)
    }
    pub fn find_best_preview(&mut self) -> Result<EmbeddedPreview, Error> {
        self.find_previews()?
            .into_iter()
            .next()
            .ok_or(Error::NoPreview)
    }
    pub fn read_preview_into(
        &mut self,
        preview: &EmbeddedPreview,
        out: &mut Vec<u8>,
    ) -> Result<(), Error> {
        self.reader.read_preview_into(preview, out)
    }
    pub fn orientation(&self) -> u16 {
        self.orientation
    }
    pub fn is_jpeg(&self) -> bool {
        self.jpeg
    }
    /// DNG and JPEG may lack a tiny embedded thumbnail. They may use the
    /// selected JPEG with reduced IDCT; Sony ARWs retain the cheap-thumb policy.
    pub fn can_reduce_main_for_thumbnail(&self) -> bool {
        self.allow_large_thumbnail
    }
    pub fn has_embedded_icc(&self) -> bool {
        self.icc_profile.is_some()
    }
    pub fn warnings(&self) -> &[String] {
        self.reader.warnings()
    }
    pub fn io_stats(&self) -> IoStats {
        self.reader.io_stats()
    }

    /// Runs only on decoded JPEG workers. Ordinary untagged/sRGB EXIF images
    /// keep the zero-conversion TurboJPEG path; explicit profiles use ColorSync.
    pub fn transform_pixels(
        &self,
        pixels: &mut [u8],
        width: u32,
        height: u32,
    ) -> Result<(), String> {
        if self.icc_profile.is_none() && !self.adobe_rgb {
            return Ok(());
        }
        let length = (width as usize)
            .checked_mul(height as usize)
            .and_then(|n| n.checked_mul(4))
            .filter(|n| *n == pixels.len() && *n <= super::decoder::MAX_DECODED_BYTES)
            .ok_or("JPEG color buffer dimensions are invalid")?;
        if length == 0
            || width as usize > super::decoder::MAX_DIMENSION
            || height as usize > super::decoder::MAX_DIMENSION
        {
            return Err("JPEG color buffer dimensions are invalid".into());
        }
        #[cfg(feature = "desktop")]
        {
            native_color(
                self.icc_profile.as_deref().unwrap_or(&[]),
                self.adobe_rgb,
                pixels,
                width,
                height,
            )
        }
        #[cfg(not(feature = "desktop"))]
        {
            Err("JPEG color conversion requires the desktop build".into())
        }
    }
}

#[cfg(feature = "desktop")]
#[allow(unsafe_code)]
fn native_color(
    profile: &[u8],
    adobe_rgb: bool,
    pixels: &mut [u8],
    width: u32,
    height: u32,
) -> Result<(), String> {
    extern "C" {
        fn fastcull_jpeg_to_srgb(
            profile: *const u8,
            profile_len: usize,
            adobe_rgb: u8,
            rgba: *mut u8,
            width: u32,
            height: u32,
        ) -> i32;
    }
    // Both buffers remain borrowed for this synchronous worker call. Native
    // conversion copies one bounded row at a time and retains no Rust pointers.
    let result = unsafe {
        fastcull_jpeg_to_srgb(
            profile.as_ptr(),
            profile.len(),
            u8::from(adobe_rgb),
            pixels.as_mut_ptr(),
            width,
            height,
        )
    };
    if result == 1 {
        Ok(())
    } else {
        Err("JPEG color profile is invalid or unsupported".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
    };
    static NEXT: AtomicU64 = AtomicU64::new(0);
    struct Fixture(PathBuf);
    impl Fixture {
        fn new(extension: &str, bytes: &[u8]) -> Self {
            let path = std::env::temp_dir().join(format!(
                "arw-sprint-source-{}-{}.{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed),
                extension
            ));
            fs::write(&path, bytes).unwrap();
            Self(path)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }
    fn jpeg(width: u16, height: u16) -> Vec<u8> {
        let mut bytes = vec![0xff, 0xd8, 0xff, 0xc0, 0, 11, 8];
        bytes.extend_from_slice(&height.to_be_bytes());
        bytes.extend_from_slice(&width.to_be_bytes());
        bytes.extend_from_slice(&[
            1, 1, 0x11, 0, 0xff, 0xda, 0, 8, 1, 1, 0, 0, 63, 0, 0x23, 0xff, 0xd9,
        ]);
        bytes
    }
    fn tiff(preview: &[u8]) -> Vec<u8> {
        let mut bytes = vec![0; 64];
        bytes[..8].copy_from_slice(b"II*\0\x08\0\0\0");
        bytes[8..10].copy_from_slice(&3u16.to_le_bytes());
        for (i, tag, kind, value) in [
            (0, 0x0112u16, 3u16, 6u32),
            (1, 0x0201, 4, 64),
            (2, 0x0202, 4, preview.len() as u32),
        ] {
            let offset = 10 + i * 12;
            bytes[offset..offset + 2].copy_from_slice(&tag.to_le_bytes());
            bytes[offset + 2..offset + 4].copy_from_slice(&kind.to_le_bytes());
            bytes[offset + 4..offset + 8].copy_from_slice(&1u32.to_le_bytes());
            bytes[offset + 8..offset + 12].copy_from_slice(&value.to_le_bytes());
        }
        bytes.extend_from_slice(preview);
        bytes
    }
    #[test]
    fn standalone_jpeg_and_dng_share_bounded_discovery_and_read() {
        let bytes = jpeg(600, 400);
        for (extension, content, offset, orientation) in [
            ("JpEg", bytes.clone(), 0, 1),
            ("dNg", tiff(&bytes), 64, 6),
            ("ARW", tiff(&bytes), 64, 6),
        ] {
            let fixture = Fixture::new(extension, &content);
            let mut source = PreviewSource::open(&fixture.0).unwrap();
            let preview = source.find_best_preview().unwrap();
            assert_eq!(
                (preview.offset, preview.width, preview.height),
                (offset, Some(600), Some(400))
            );
            assert_eq!(source.orientation(), orientation);
            let mut extracted = vec![1; 1024];
            source.read_preview_into(&preview, &mut extracted).unwrap();
            assert_eq!(extracted, bytes);
        }
    }
    #[test]
    fn jpeg_exif_thumbnail_is_relative_to_app1_and_primary_stays_full_image() {
        let full = jpeg(600, 400);
        let thumbnail = jpeg(80, 60);
        let tiff = tiff(&thumbnail);
        let mut bytes = vec![0xff, 0xd8, 0xff, 0xe1];
        bytes.extend_from_slice(&((tiff.len() + 8) as u16).to_be_bytes());
        bytes.extend_from_slice(b"Exif\0\0");
        bytes.extend_from_slice(&tiff);
        bytes.extend_from_slice(&full[2..]);
        let fixture = Fixture::new("jpg", &bytes);
        let mut source = PreviewSource::open(&fixture.0).unwrap();
        let primary = source.find_previews().unwrap();
        assert_eq!(
            primary.len(),
            1,
            "EXIF thumbnail must never replace main JPEG"
        );
        assert_eq!(primary[0].width, Some(600));
        let candidates = source.find_thumbnail_previews().unwrap();
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0].offset, 0);
        assert_eq!(candidates[1].offset, 12 + 64);
        assert_eq!(source.orientation(), 6);
        let mut extracted = Vec::new();
        source
            .read_preview_into(&candidates[1], &mut extracted)
            .unwrap();
        assert_eq!(extracted, thumbnail);
    }
    #[test]
    fn missing_dng_preview_and_truncated_jpeg_fail_without_guessing_payloads() {
        let fixture = Fixture::new("dng", b"II*\0\x08\0\0\0\0\0\0\0\0\0");
        let mut source = PreviewSource::open(&fixture.0).unwrap();
        assert!(matches!(source.find_previews(), Err(Error::NoPreview)));
        let mut bytes = jpeg(60, 40);
        bytes.pop();
        let fixture = Fixture::new("jpeg", &bytes);
        assert!(PreviewSource::open(&fixture.0)
            .unwrap()
            .find_previews()
            .is_err());
    }
    #[cfg(feature = "desktop")]
    #[test]
    fn adobe_rgb_conversion_changes_colors_without_changing_alpha_or_size() {
        let mut pixels = [64, 128, 192, 255, 255, 255, 255, 255];
        native_color(&[], true, &mut pixels, 2, 1).unwrap();
        assert_ne!(&pixels[..3], &[64, 128, 192]);
        assert_eq!(pixels[3], 255);
        assert!(pixels[4..].iter().all(|value| *value >= 254));
        assert!(native_color(&[0; 128], false, &mut pixels, 2, 1).is_err());
    }
    #[cfg(feature = "turbo")]
    #[test]
    fn corrupt_main_jpeg_cannot_be_replaced_by_its_valid_exif_thumbnail() {
        use crate::image::decoder::{Backend, Decoder};
        let mut compressor = turbojpeg::Compressor::new().unwrap();
        let mut encode = |width: usize, height: usize| {
            let pixels = vec![128; width * height * 3];
            compressor
                .compress_to_vec(turbojpeg::Image {
                    pixels: &pixels,
                    width,
                    pitch: width * 3,
                    height,
                    format: turbojpeg::PixelFormat::RGB,
                })
                .unwrap()
        };
        let mut full = encode(600, 400);
        let thumbnail = encode(80, 60);
        let sos = full
            .windows(2)
            .position(|bytes| bytes == [0xff, 0xda])
            .unwrap();
        full[sos + 5] = 4; // Unknown component: framed JPEG, invalid entropy setup.
        let tiff = tiff(&thumbnail);
        let mut bytes = vec![0xff, 0xd8, 0xff, 0xe1];
        bytes.extend_from_slice(&((tiff.len() + 8) as u16).to_be_bytes());
        bytes.extend_from_slice(b"Exif\0\0");
        bytes.extend_from_slice(&tiff);
        bytes.extend_from_slice(&full[2..]);
        let fixture = Fixture::new("jpg", &bytes);
        let mut source = PreviewSource::open(&fixture.0).unwrap();
        let main = source.find_previews().unwrap();
        assert_eq!(main.len(), 1);
        let mut encoded = Vec::new();
        source.read_preview_into(&main[0], &mut encoded).unwrap();
        let mut decoder = Decoder::new(Backend::Turbo).unwrap();
        assert!(decoder.decode(&encoded).is_err());
        let filmstrip = source.find_thumbnail_previews().unwrap();
        source
            .read_preview_into(&filmstrip[1], &mut encoded)
            .unwrap();
        let valid = decoder.decode(&encoded).unwrap();
        assert_eq!((valid.width, valid.height), (80, 60));
    }
}
