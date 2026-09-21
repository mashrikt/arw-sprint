use std::cmp::Ordering;
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

/// Enumerate one folder without recursion or opening each RAW file.
///
/// Only entries with an ARW extension need a file-type lookup. The GUI must call
/// this on its directory worker, never on its event thread.
pub fn scan_arw_paths(path: &Path) -> io::Result<Vec<PathBuf>> {
    let metadata = fs::metadata(path)?;
    if metadata.is_file() {
        return if is_arw(path) {
            Ok(vec![path.to_path_buf()])
        } else {
            Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "input file must have an .ARW extension",
            ))
        };
    }
    if !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "input must be an ARW file or directory",
        ));
    }

    let mut paths = Vec::new();
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let path = entry.path();
        if is_arw(&path) && entry.file_type()?.is_file() {
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
}
