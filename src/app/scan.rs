use crate::browser::directory::{is_arw, natural_filename_cmp};
use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};

pub struct ScanResult {
    pub session: u64,
    pub folder: PathBuf,
    pub files: Arc<Vec<PathBuf>>,
    pub requested: Option<PathBuf>,
    pub complete: bool,
    pub elapsed: Duration,
}

type ScanCallback = Arc<dyn Fn(Result<ScanResult, (u64, String)>) + Send + Sync>;

/// Provisional enumeration order must not replace the final first photo or an
/// explicitly opened ARW. Only an intentional navigation preserves selection.
pub fn selected_index(
    files: &[PathBuf],
    requested: Option<&PathBuf>,
    previous: Option<&PathBuf>,
    user_navigated: bool,
) -> usize {
    let preferred = if user_navigated {
        previous.or(requested)
    } else {
        requested
    };
    preferred
        .and_then(|path| files.iter().position(|file| file == path))
        .unwrap_or(0)
}

/// Latest-session cancellation prevents a slow previous folder from replacing
/// the current list. Directory enumeration is always off the event thread.
pub fn start(path: PathBuf, session: u64, current: Arc<AtomicU64>, wake: ScanCallback) {
    let spawn_wake = Arc::clone(&wake);
    if let Err(error) = thread::Builder::new()
        .name("fastcull-directory".into())
        .spawn(move || {
            let start = Instant::now();
            let run = || -> Result<(), String> {
                let path = std::path::absolute(&path).map_err(|e| e.to_string())?;
                let metadata =
                    std::fs::metadata(&path).map_err(|e| format!("{}: {e}", path.display()))?;
                let requested = if metadata.is_file() && is_arw(&path) {
                    Some(path.clone())
                } else {
                    None
                };
                let folder = if requested.is_some() {
                    path.parent()
                        .ok_or("file has no parent folder")?
                        .to_path_buf()
                } else if metadata.is_dir() {
                    path.clone()
                } else {
                    return Err("Open a folder or a Sony .ARW photograph.".into());
                };
                let mut files = Vec::new();
                let mut early_sent = false;
                for entry in std::fs::read_dir(&folder).map_err(|e| e.to_string())? {
                    if current.load(Ordering::Acquire) != session {
                        return Ok(());
                    }
                    let entry = entry.map_err(|e| e.to_string())?;
                    let path = entry.path();
                    if is_arw(&path) && entry.file_type().map_err(|e| e.to_string())?.is_file() {
                        files.push(path);
                    }
                    if !early_sent
                        && files.len() >= 32
                        && start.elapsed() >= Duration::from_millis(40)
                    {
                        sort(&mut files);
                        wake(Ok(ScanResult {
                            session,
                            folder: folder.clone(),
                            files: Arc::new(files.clone()),
                            requested: requested.clone(),
                            complete: false,
                            elapsed: start.elapsed(),
                        }));
                        early_sent = true;
                    }
                }
                if current.load(Ordering::Acquire) != session {
                    return Ok(());
                }
                sort(&mut files);
                wake(Ok(ScanResult {
                    session,
                    folder,
                    files: Arc::new(files),
                    requested,
                    complete: true,
                    elapsed: start.elapsed(),
                }));
                Ok(())
            };
            if let Err(error) = run() {
                if current.load(Ordering::Acquire) == session {
                    wake(Err((session, error)));
                }
            }
        })
    {
        spawn_wake(Err((
            session,
            format!("Could not start directory scanner: {error}"),
        )));
    }
}

fn sort(files: &mut [PathBuf]) {
    files.sort_unstable_by(|a, b| {
        natural_filename_cmp(
            a.file_name().unwrap_or(a.as_os_str()),
            b.file_name().unwrap_or(b.as_os_str()),
        )
        .then_with(|| a.cmp(b))
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn final_scan_restores_natural_first_unless_user_navigated() {
        let files: Vec<PathBuf> = ["DSC00001.ARW", "DSC00002.ARW", "DSC00003.ARW"]
            .into_iter()
            .map(PathBuf::from)
            .collect();
        assert_eq!(selected_index(&files, None, Some(&files[2]), false), 0);
        assert_eq!(selected_index(&files, None, Some(&files[2]), true), 2);
        assert_eq!(selected_index(&[], None, None, false), 0);
    }
    #[test]
    fn explicitly_opened_photo_wins_over_a_provisional_first() {
        let first = PathBuf::from("DSC00001.ARW");
        let requested = PathBuf::from("DSC00993.ARW");
        assert_eq!(
            selected_index(std::slice::from_ref(&first), Some(&requested), None, false),
            0
        );
        assert_eq!(
            selected_index(
                &[first.clone(), requested.clone()],
                Some(&requested),
                Some(&first),
                false
            ),
            1
        );
    }
}
