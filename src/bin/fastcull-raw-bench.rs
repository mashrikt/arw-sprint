//! Read-only benchmark of the exact native sensor-rendering path used by the app.
use fastcull::image::raw::RawDecoder;
use std::{
    fs::OpenOptions,
    io::{BufWriter, Write},
    path::{Path, PathBuf},
    process::ExitCode,
    time::{Duration, Instant},
};

const HELP: &str = "fastcull-raw-bench [--repeat N] [--cancel-test] [--output NEW.ppm] FILE.ARW ...

Benchmark full-resolution RAW sensor rendering, not embedded JPEG extraction.
Sony ARWs and supported DNGs only. Files and sidecars are never modified.

  --repeat N     Sequential passes, 1..100 (default 1)
  --cancel-test  Request cancellation after 50 ms, then verify normal recovery
  --output PATH  Write the first completed render as a NEW PPM (never overwrite)
  -h, --help     Show this help

Output: metadata-probe and sensor-decode time, dimensions, output and scratch
memory estimates. Includes allocation, unpacking, PPG demosaic and sRGB RGBA8
conversion; excludes GPU upload and display. Run the release build.
The desktop permits 384 MiB temporary RAW scratch; this diagnostic permits the
bridge's 768 MiB ceiling, so a large diagnostic decode may not fit the desktop.
At most 1000 renders. Camera and RAW compression support vary by file.";

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("fastcull-raw-bench: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let mut arguments = std::env::args_os().skip(1);
    let mut paths = Vec::<PathBuf>::new();
    let mut repeat = 1usize;
    let mut cancel_test = false;
    let mut output = None;
    let mut positional = false;
    while let Some(argument) = arguments.next() {
        match argument.to_str().filter(|_| !positional) {
            Some("-h" | "--help") => {
                println!("{HELP}");
                return Ok(());
            }
            Some("--") => positional = true,
            Some("--cancel-test") => cancel_test = true,
            Some("--repeat") => {
                repeat = arguments
                    .next()
                    .and_then(|value| value.into_string().ok())
                    .and_then(|value| value.parse().ok())
                    .filter(|value| (1..=100).contains(value))
                    .ok_or("--repeat needs a number from 1 to 100")?;
            }
            Some("--output") => {
                output = Some(PathBuf::from(
                    arguments.next().ok_or("--output needs a new PPM path")?,
                ));
            }
            Some(flag) if flag.starts_with('-') => return Err(format!("unknown option {flag}")),
            _ => paths.push(argument.into()),
        }
    }
    if paths.is_empty() || paths.len().saturating_mul(repeat) > 1000 {
        return Err("provide RAW file paths, with at most 1000 total renders; see --help".into());
    }
    let mut decoder = RawDecoder::new()?;
    if cancel_test {
        decoder.probe(&paths[0])?;
        let start = Instant::now();
        let result =
            decoder.decode_cancellable(&paths[0], &|| start.elapsed() >= Duration::from_millis(50));
        match result {
            Err(error) if error.to_lowercase().contains("cancel") => println!(
                "Cancellation: PASS after {:.1} ms; next decode checks recovery",
                start.elapsed().as_secs_f64() * 1000.0
            ),
            Err(error) => {
                return Err(format!(
                    "cancellation test failed for another reason: {error}"
                ))
            }
            Ok(_) => return Err("render finished before the cancellation test fired".into()),
        }
    }
    let mut totals = Vec::new();
    for pass in 0..repeat {
        for path in &paths {
            let start = Instant::now();
            let info = decoder.probe(path)?;
            let probe_ms = start.elapsed().as_secs_f64() * 1000.0;
            let decode_start = Instant::now();
            let decoded = decoder.decode(path)?;
            let decode_ms = decode_start.elapsed().as_secs_f64() * 1000.0;
            if decoded.info != info || decoded.pixels.len() != info.bytes {
                return Err("decoded image does not match its probed allocation".into());
            }
            let total_ms = start.elapsed().as_secs_f64() * 1000.0;
            totals.push(total_ms);
            println!(
                "{} pass {}: {}x{}, orientation {}, probe {:.2} ms, RAW {:.2} ms, total {:.2} ms, output {:.2} MiB, native scratch estimate {:.2} MiB",
                path.display(), pass + 1, info.width, info.height, info.orientation,
                probe_ms, decode_ms, total_ms, info.bytes as f64 / 1048576.0,
                (info.working_bytes - info.bytes) as f64 / 1048576.0
            );
            if let Some(path) = output.take() {
                write_ppm(&path, info.width, info.height, &decoded.pixels)?;
            }
        }
    }
    totals.sort_by(f64::total_cmp);
    let percentile = |p: usize| totals[(totals.len() * p).div_ceil(100).saturating_sub(1)];
    println!(
        "{} completed renders; total p50 {:.2} ms, p95 {:.2} ms, p99 {:.2} ms",
        totals.len(),
        percentile(50),
        percentile(95),
        percentile(99)
    );
    Ok(())
}

fn write_ppm(path: &Path, width: u32, height: u32, pixels: &[u8]) -> Result<(), String> {
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|e| format!("new output {}: {e}", path.display()))?;
    let mut file = BufWriter::new(file);
    writeln!(file, "P6\n{width} {height}\n255").map_err(|e| e.to_string())?;
    for pixel in pixels.chunks_exact(4) {
        file.write_all(&pixel[..3]).map_err(|e| e.to_string())?;
    }
    file.flush().map_err(|e| e.to_string())
}
