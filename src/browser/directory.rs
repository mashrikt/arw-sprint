use std::cmp::Ordering;
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

/// Enumerate ARWs for the extraction and benchmark commands. The desktop uses
/// `scan_photo_paths`/`is_photo`; benchmarks retain their ARW-only meaning.
pub fn scan_arw_paths(path: &Path) -> io::Result<Vec<PathBuf>> {
    scan_paths(
        path,
        |path| is_visible_photo_name(path) && is_arw(path),
        "input must be a visible ARW file or directory",
    )
}

/// Enumerate one folder without recursion or opening image payloads.
pub fn scan_photo_paths(path: &Path) -> io::Result<Vec<PathBuf>> {
    scan_paths(
        path,
        is_photo,
        "input must be an ARW, DNG, JPEG file or directory",
    )
}

fn scan_paths(
    path: &Path,
    accepts: impl Fn(&Path) -> bool,
    error: &'static str,
) -> io::Result<Vec<PathBuf>> {
    let metadata = fs::metadata(path)?;
    if metadata.is_file() {
        return if accepts(path) {
            Ok(vec![path.to_path_buf()])
        } else {
            Err(io::Error::new(io::ErrorKind::InvalidInput, error))
        };
    }
    if !metadata.is_dir() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, error));
    }
    let mut paths = Vec::new();
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let path = entry.path();
        if accepts(&path) && entry.file_type()?.is_file() {
            paths.push(path);
        }
    }
    paths.sort_unstable_by(|left, right| {
        natural_filename_cmp(
            left.file_name().unwrap_or_else(|| left.as_os_str()),
            right.file_name().unwrap_or_else(|| right.as_os_str()),
        )
        .then_with(|| left.cmp(right))
    });
    Ok(paths)
}

fn is_visible_photo_name(path: &Path) -> bool {
    path.file_name().is_some_and(|name| {
        let name = name.as_bytes();
        // AppleDouble companions have the original photograph's extension.
        // Other cache/temp suffixes already fail the extension allow-list.
        // Do not guess from image size or words such as "cache" in real names.
        !name.starts_with(b".") && !name.starts_with(b"~$")
    })
}

pub fn is_photo(path: &Path) -> bool {
    is_visible_photo_name(path)
        && path.extension().is_some_and(|extension| {
            [b"arw".as_slice(), b"dng", b"jpg", b"jpeg"]
                .iter()
                .any(|kind| extension.as_bytes().eq_ignore_ascii_case(kind))
        })
}

pub fn is_jpeg(path: &Path) -> bool {
    path.extension().is_some_and(|extension| {
        extension.as_bytes().eq_ignore_ascii_case(b"jpg")
            || extension.as_bytes().eq_ignore_ascii_case(b"jpeg")
    })
}

pub fn is_arw(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| extension.as_bytes().eq_ignore_ascii_case(b"arw"))
}

/// Compare digit runs by magnitude without parsing them into an integer.
/// Invalid UTF-8 filenames are supported; ASCII letters compare without case.
pub fn natural_filename_cmp(left: &OsStr, right: &OsStr) -> Ordering {
    let (left, right) = (left.as_bytes(), right.as_bytes());
    let (mut a, mut b) = (0, 0);
    while a < left.len() && b < right.len() {
        if left[a].is_ascii_digit() && right[b].is_ascii_digit() {
            let (a_start, b_start) = (a, b);
            while a < left.len() && left[a].is_ascii_digit() {
                a += 1;
            }
            while b < right.len() && right[b].is_ascii_digit() {
                b += 1;
            }
            let a_number = significant_digits(&left[a_start..a]);
            let b_number = significant_digits(&right[b_start..b]);
            let order = a_number
                .len()
                .cmp(&b_number.len())
                .then_with(|| a_number.cmp(b_number))
                .then_with(|| (a - a_start).cmp(&(b - b_start)));
            if order != Ordering::Equal {
                return order;
            }
        } else {
            let order = left[a]
                .to_ascii_lowercase()
                .cmp(&right[b].to_ascii_lowercase());
            if order != Ordering::Equal {
                return order;
            }
            a += 1;
            b += 1;
        }
    }
    (left.len() - a)
        .cmp(&(right.len() - b))
        .then_with(|| left.cmp(right))
}

fn significant_digits(digits: &[u8]) -> &[u8] {
    let first_nonzero = digits
        .iter()
        .position(|digit| *digit != b'0')
        .unwrap_or(digits.len());
    &digits[first_nonzero..]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    #[test]
    fn sorts_numbers_naturally_and_deterministically() {
        let mut names = [
            "DSC10.ARW",
            "DSC2.ARW",
            "DSC0001.ARW",
            "DSC1.ARW",
            "DSC01.ARW",
        ];
        names.sort_by(|a, b| natural_filename_cmp(OsStr::new(a), OsStr::new(b)));
        assert_eq!(
            names,
            [
                "DSC1.ARW",
                "DSC01.ARW",
                "DSC0001.ARW",
                "DSC2.ARW",
                "DSC10.ARW"
            ]
        );
    }

    #[test]
    fn arbitrary_length_digit_runs_do_not_overflow() {
        let smaller = format!("DSC{}.ARW", "9".repeat(200));
        let larger = format!("DSC1{}.ARW", "0".repeat(200));
        assert_eq!(
            natural_filename_cmp(OsStr::new(&smaller), OsStr::new(&larger)),
            Ordering::Less
        );
    }

    #[test]
    fn non_utf8_filenames_and_mixed_case_extensions_are_supported() {
        let a = OsString::from_vec(b"\xffDSC2.ArW".to_vec());
        let b = OsString::from_vec(b"\xffDSC10.arw".to_vec());
        assert!(is_arw(Path::new(&a)));
        assert_eq!(natural_filename_cmp(&a, &b), Ordering::Less);
        assert!(!is_arw(Path::new("DSC1.ARW.jpg")));
    }

    #[test]
    fn comparisons_are_antisymmetric_and_transitive() {
        let names = [
            "", "a", "A", "a0", "a00", "a1", "A1", "a01", "a1a", "a01b", "a2", "a10",
        ];
        for a in names {
            for b in names {
                let ab = natural_filename_cmp(OsStr::new(a), OsStr::new(b));
                assert_eq!(
                    ab,
                    natural_filename_cmp(OsStr::new(b), OsStr::new(a)).reverse()
                );
                for c in names {
                    if ab != Ordering::Greater
                        && natural_filename_cmp(OsStr::new(b), OsStr::new(c)) != Ordering::Greater
                    {
                        assert_ne!(
                            natural_filename_cmp(OsStr::new(a), OsStr::new(c)),
                            Ordering::Greater
                        );
                    }
                }
            }
        }
    }
    #[test]
    fn photo_filter_accepts_mixed_formats_without_hiding_real_cache_names() {
        for name in [
            "DSC1.ARW",
            "DSC2.dNg",
            "DSC3.JpG",
            "DSC4.JPEG",
            "cache.jpg",
            "thumbnail.ARW",
            "small.jpeg",
        ] {
            assert!(is_photo(Path::new(name)), "{name}");
        }
        for name in [
            "._DSC1.ARW",
            ".DSC2.jpg",
            "~$DSC3.dng",
            "DSC1.ARW.tmp",
            "DSC1.jpg~",
            "DSC1.xmp",
            "DSC1.arw.cache",
            "DSC1.db",
            "_Rejected",
        ] {
            assert!(!is_photo(Path::new(name)), "{name}");
        }
    }

    #[test]
    fn folder_scan_filters_companions_and_retains_arw_only_benchmark_semantics() {
        let directory =
            std::env::temp_dir().join(format!("arw-sprint-formats-{}", std::process::id()));
        std::fs::create_dir(&directory).unwrap();
        struct Cleanup(PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(directory.clone());
        for name in [
            "DSC10.ARW",
            "DSC2.DNG",
            "DSC1.jpg",
            "DSC3.jpeg",
            "._DSC10.ARW",
            ".preview.jpg",
            "DSC10.ARW.tmp",
        ] {
            std::fs::write(directory.join(name), []).unwrap();
        }
        std::fs::create_dir(directory.join("folder.ARW")).unwrap();
        let photos = scan_photo_paths(&directory).unwrap();
        assert_eq!(
            photos
                .iter()
                .map(|p| p.file_name().unwrap().to_str().unwrap())
                .collect::<Vec<_>>(),
            ["DSC1.jpg", "DSC2.DNG", "DSC3.jpeg", "DSC10.ARW"]
        );
        assert_eq!(
            scan_arw_paths(&directory).unwrap(),
            [directory.join("DSC10.ARW")]
        );
        assert!(scan_photo_paths(&directory.join("._DSC10.ARW")).is_err());
    }
}
