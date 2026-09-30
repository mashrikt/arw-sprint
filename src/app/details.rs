//! Read-only presentation of metadata already delivered by the I/O worker.
use super::{App, PhotoInfo};
use std::{fmt::Write, path::Path};

impl App {
    pub(super) fn toggle_photo_details(&mut self) {
        self.photo_details_visible = !self.photo_details_visible;
        if let Some(menu) = &self.menu {
            menu.set_photo_details_visible(self.photo_details_visible);
        }
        self.refresh_photo_details();
        self.redraw();
    }

    pub(super) fn refresh_photo_details(&mut self) {
        if !self.photo_details_visible {
            if let Some(renderer) = &mut self.renderer {
                renderer.set_photo_details(None);
            }
            return;
        }
        let content = self.current_photo_details();
        if let Some(renderer) = &mut self.renderer {
            renderer.set_photo_details(content.as_deref());
        }
    }

    pub(super) fn current_photo_details(&self) -> Option<String> {
        let selected = self.current();
        panel_path(selected.as_deref(), self.displayed.as_deref()).map(|path| {
            let dimensions = self
                .textures
                .iter()
                .find(|(p, _)| p == path)
                .map(|(_, texture)| (texture.original_width, texture.original_height));
            format_details(path, self.info.get(path), dimensions)
        })
    }
}

/// During an uncached transition the old image stays visible. Its details must
/// stay with it until `show` installs the new image. Comparison uses this same
/// displayed path for the right pane; it never borrows the pinned photo's EXIF.
fn panel_path<'a>(selected: Option<&'a Path>, displayed: Option<&'a Path>) -> Option<&'a Path> {
    selected.map(|selected| displayed.unwrap_or(selected))
}

fn clean_text(text: &str, limit: usize) -> String {
    // TIFF strings and filenames are untrusted. No control character may create
    // extra rows in the native card's tab/newline-delimited presentation format.
    let mut chars = text.chars().map(|c| if c.is_control() { ' ' } else { c });
    let mut result: String = chars.by_ref().take(limit).collect();
    if chars.next().is_some() {
        result.push('…');
    }
    result.trim().to_owned()
}

fn number(value: f64) -> String {
    if value.fract().abs() < 0.000_001 {
        format!("{value:.0}")
    } else {
        let text = format!("{value:.2}");
        text.trim_end_matches('0').trim_end_matches('.').to_owned()
    }
}

fn positive(value: Option<f64>) -> Option<f64> {
    value.filter(|n| n.is_finite() && *n > 0.0)
}

fn format_details(path: &Path, info: Option<&PhotoInfo>, dimensions: Option<(u32, u32)>) -> String {
    let mut text = clean_text(&path.file_name().unwrap_or_default().to_string_lossy(), 128);
    let metadata = info.and_then(|info| info.metadata.as_ref());
    let mut fields = 0;
    let mut row = |label: &str, value: String| {
        if !value.is_empty() {
            let _ = write!(text, "\n{label}\t{value}");
            fields += 1;
        }
    };
    if let Some(metadata) = metadata {
        if let Some(camera) = &metadata.camera_model {
            row("Camera", clean_text(camera, 160));
        }
        if let Some(lens) = &metadata.lens_model {
            row("Lens", clean_text(lens, 160));
        }
        if let Some(captured) = &metadata.captured_at {
            let mut date = clean_text(captured, 64);
            // EXIF uses colons in dates. Keep the camera's local time; no timezone
            // is recorded in this subset, so never imply a UTC conversion.
            if date.len() >= 19
                && date.as_bytes()[4] == b':'
                && date.as_bytes()[7] == b':'
                && date.as_bytes()[..19].is_ascii()
            {
                date.replace_range(7..8, "-");
                date.replace_range(4..5, "-");
            }
            row("Captured", date);
        }
        if let Some(exposure) = positive(metadata.exposure_time) {
            row(
                "Shutter",
                if exposure <= 0.5 {
                    format!("1/{} s", number(1.0 / exposure))
                } else {
                    format!("{} s", number(exposure))
                },
            );
        }
        if let Some(aperture) = positive(metadata.aperture) {
            row("Aperture", format!("f/{}", number(aperture)));
        }
        if let Some(iso) = metadata.iso.filter(|iso| *iso > 0) {
            row("ISO", iso.to_string());
        }
        if let Some(focal) = positive(metadata.focal_length) {
            row("Focal length", format!("{} mm", number(focal)));
        }
    }
    let has_exif = fields != 0;
    if let Some((width, height)) = dimensions {
        let _ = write!(text, "\nPreview size\t{width} × {height}");
    }
    if metadata.is_none() {
        text.push_str(if info.is_some_and(|info| info.read) {
            "\nStatus\tEXIF unavailable"
        } else {
            "\nStatus\tLoading EXIF…"
        });
    } else if !has_exif {
        text.push_str("\nStatus\tNo EXIF details recorded");
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metadata::ExifMetadata;

    #[test]
    fn follows_displayed_photo_during_navigation_and_clears_for_empty_filter() {
        let selected = Path::new("next.ARW");
        let shown = Path::new("previous.ARW");
        assert_eq!(panel_path(Some(selected), Some(shown)), Some(shown));
        assert_eq!(panel_path(Some(selected), None), Some(selected));
        assert_eq!(panel_path(None, Some(shown)), None);
    }

    #[test]
    fn formats_camera_fields_and_preserves_long_exposures_and_fractional_focal_lengths() {
        let info = PhotoInfo {
            metadata: Some(ExifMetadata {
                camera_model: Some("ILCE-7M5".into()),
                lens_model: Some("FE 200-600mm F5.6-6.3 G OSS".into()),
                captured_at: Some("2026:09:20 06:14:23".into()),
                exposure_time: Some(1.3),
                aperture: Some(6.3),
                iso: Some(1600),
                focal_length: Some(24.5),
                ..Default::default()
            }),
            read: true,
            ..Default::default()
        };
        let content = format_details(Path::new("DSC00993.ARW"), Some(&info), Some((7008, 4672)));
        assert_eq!(content, "DSC00993.ARW\nCamera\tILCE-7M5\nLens\tFE 200-600mm F5.6-6.3 G OSS\nCaptured\t2026-09-20 06:14:23\nShutter\t1.3 s\nAperture\tf/6.3\nISO\t1600\nFocal length\t24.5 mm\nPreview size\t7008 × 4672");
    }

    #[test]
    fn formats_fast_shutter_and_omits_invalid_numbers() {
        let info = PhotoInfo {
            metadata: Some(ExifMetadata {
                exposure_time: Some(1.0 / 2000.0),
                aperture: Some(f64::NAN),
                iso: Some(0),
                focal_length: Some(f64::INFINITY),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(
            format_details(Path::new("photo.ARW"), Some(&info), None),
            "photo.ARW\nShutter\t1/2000 s"
        );
    }

    #[test]
    fn distinguishes_pending_failed_and_empty_metadata() {
        let path = Path::new("photo.ARW");
        assert!(format_details(path, None, None).ends_with("Loading EXIF…"));
        let mut info = PhotoInfo {
            read: true,
            ..Default::default()
        };
        assert!(format_details(path, Some(&info), None).ends_with("EXIF unavailable"));
        info.metadata = Some(ExifMetadata::default());
        assert!(format_details(path, Some(&info), None).ends_with("No EXIF details recorded"));
    }

    #[test]
    fn untrusted_strings_cannot_add_rows_and_unicode_truncation_is_safe() {
        assert_eq!(
            clean_text(" camera\tname\nISO\r1600\0 ", 100),
            "camera name ISO 1600"
        );
        assert_eq!(clean_text("🦁".repeat(200).as_str(), 3), "🦁🦁🦁…");
        let info = PhotoInfo {
            metadata: Some(ExifMetadata {
                camera_model: Some("fake\nISO\t999".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(
            format_details(Path::new("photo\nname.ARW"), Some(&info), None),
            "photo name.ARW\nCamera\tfake ISO 999"
        );
    }
}
