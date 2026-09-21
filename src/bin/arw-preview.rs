use fastcull::arw::{Error as ArwError, PreviewReader};
use std::{
    env,
    ffi::OsString,
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    process::ExitCode,
    time::Instant,
};

const HELP: &str = "arw-preview <input.ARW> <output.jpg> [--list]\n\nExtract the largest metadata-declared embedded JPEG; never decode RAW.\n--list prints all usable previews before extraction. Existing output files\nare never overwritten. FASTCULL_LOG=1 enables parser diagnostics.\n";

fn run(args: impl IntoIterator<Item = OsString>) -> Result<(), String> {
    let mut paths = Vec::new();
    let mut list = false;
    let mut positional = false;
    for arg in args {
        match arg.to_str() {
            Some("--help" | "-h") if !positional => {
                print!("{HELP}");
                return Ok(());
            }
            Some("--list") if !positional => list = true,
            Some("--") if !positional => positional = true,
            Some(s) if !positional && s.starts_with('-') => {
                return Err(format!("unknown option {s}"))
            }
            _ => paths.push(PathBuf::from(arg)),
        }
    }
    if paths.len() != 2 {
        return Err(HELP.to_owned());
    }
    if paths[1]
        .extension()
        .is_none_or(|ext| !ext.eq_ignore_ascii_case("jpg") && !ext.eq_ignore_ascii_case("jpeg"))
    {
        return Err("output must have .jpg or .jpeg extension".into());
    }
    let total = Instant::now();
    let mut reader =
        PreviewReader::open(&paths[0]).map_err(|e| input_error("opening input", &paths[0], e))?;
    let open = total.elapsed();
    let start = Instant::now();
    let previews_result = reader.find_previews();
    let parse = start.elapsed();
    let metadata_io = reader.io_stats();
    if env::var_os("FASTCULL_LOG").is_some() {
        for warning in reader.warnings() {
            eprintln!("parser: {warning}");
        }
    }
    let previews =
        previews_result.map_err(|e| input_error("discovering previews", &paths[0], e))?;
    if list {
        for (i, p) in previews.iter().enumerate() {
            println!(
                "candidate {}: {}x{} offset={} length={}",
                i + 1,
                p.width.unwrap_or(0),
                p.height.unwrap_or(0),
                p.offset,
                p.length
            );
        }
    }
    let start = Instant::now();
    let mut jpeg = Vec::new();
    let mut chosen = None;
    for preview in previews {
        match reader.read_preview_into(&preview, &mut jpeg) {
            Ok(()) => {
                chosen = Some(preview);
                break;
            }
            Err(e) => eprintln!(
                "preview at {} rejected: {}",
                preview.offset,
                input_error("reading embedded JPEG", &paths[0], e)
            ),
        }
    }
    let preview = chosen.ok_or("all declared previews failed JPEG framing validation")?;
    let read = start.elapsed();
    let start = Instant::now();
    write_new(&paths[1], &jpeg)
        .map_err(|e| format!("writing output '{}': {e}", paths[1].display()))?;
    let write = start.elapsed();
    let stats = reader.io_stats();
    println!("{} -> {}\n{}x{}, {} JPEG bytes at offset {}\nopen: {:.3} ms; discovery: {:.3} ms; JPEG read + framing: {:.3} ms; output + sync: {:.3} ms; total: {:.3} ms\nmetadata reads: {} bytes / {} calls; all reads: {} bytes / {} calls; ARW size: {} bytes",
        paths[0].display(), paths[1].display(), preview.width.unwrap_or(0), preview.height.unwrap_or(0), jpeg.len(), preview.offset,
        open.as_secs_f64()*1000.0, parse.as_secs_f64()*1000.0, read.as_secs_f64()*1000.0, write.as_secs_f64()*1000.0, total.elapsed().as_secs_f64()*1000.0,
        metadata_io.bytes_read, metadata_io.read_calls, stats.bytes_read, stats.read_calls, reader.file_len());
    Ok(())
}

fn input_error(operation: &str, path: &Path, error: ArwError) -> String {
    let mut message = format!("{operation} '{}': {error}", path.display());
    if matches!(&error, ArwError::Io(e) if e.kind() == io::ErrorKind::PermissionDenied
        || matches!(e.raw_os_error(), Some(1 | 13)))
    {
        message.push_str(
            "\nmacOS denied access to the input. Check System Settings > Privacy & Security > Files & Folders for the terminal app running this command, and enable access to the input's location (for example, Desktop Folder). Then quit and reopen that terminal app.",
        );
    }
    message
}

fn write_new(path: &Path, bytes: &[u8]) -> io::Result<()> {
    // create_new also protects aliases/hardlinks to existing inputs. Failure never
    // truncates an existing file. Remove only our newly created output on I/O error.
    let mut output = OpenOptions::new().write(true).create_new(true).open(path)?;
    if let Err(error) = output.write_all(bytes).and_then(|_| output.sync_all()) {
        drop(output);
        let _ = fs::remove_file(path);
        return Err(error);
    }
    Ok(())
}

fn main() -> ExitCode {
    match run(env::args_os().skip(1)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("arw-preview: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn output_creation_never_overwrites_an_existing_file() {
        let path = env::temp_dir().join(format!("fastcull-output-{}.jpg", std::process::id()));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        file.write_all(b"existing user data").unwrap();
        drop(file);
        let result = write_new(&path, b"new jpeg");
        let actual = fs::read(&path).unwrap();
        fs::remove_file(&path).unwrap();
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(actual, b"existing user data");
    }
    #[test]
    fn rejects_raw_output_name_before_reading_input() {
        let error = run([OsString::from("missing.ARW"), OsString::from("output.ARW")]).unwrap_err();
        assert!(error.contains("output must have"));
    }
}
