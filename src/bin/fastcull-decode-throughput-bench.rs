//! Isolate JPEG decode throughput from disk, UI, upload, and cache scheduling.
use fastcull::arw::PreviewReader;
use fastcull::browser::directory::scan_arw_paths;
use fastcull::image::decoder::{Backend, Decoder};
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

const MAX_CORPUS_BYTES: usize = 256 * 1024 * 1024;
const MAX_SAMPLES: usize = 10_000;
type Corpus = Arc<Vec<Vec<u8>>>;
const HELP: &str = "fastcull-decode-throughput-bench <folder-or-file.ARW> [options]

Measure parallel TurboJPEG decoding from a fixed, memory-resident JPEG corpus.

  --limit N       First N naturally sorted files (default 12; range 1..500)
  --passes N      Timed passes per combination (default 3; range 1..100)
  --workers LIST Total concurrent decoders: 1,2,3 (default 1,2,3)
  --scales LIST  JPEG size denominators: 1,2,4 (default 1,2,4)
  --reverse      Reverse combination order for a second comparison run
  -h, --help     Show this help

Build with --release --features turbo. No photos or sidecars are modified.
At most 256 MiB of compressed JPEGs and 10000 timed decodes per combination.
Each worker warms up once, owns a decoder, and transfers/drops its RGBA buffer
after each decode, matching the desktop's fresh output allocation behavior.
All listed workers decode concurrently; none is reserved for the foreground.
This measures memory-only decode, not disk, navigation, GPU, or display latency.
Close other heavy tasks and repeat with --reverse to check order/thermal effects.";

#[derive(Debug, PartialEq, Eq)]
struct Args {
    path: PathBuf,
    limit: usize,
    passes: usize,
    workers: Vec<usize>,
    scales: Vec<usize>,
    reverse: bool,
}

impl Args {
    fn parse(arguments: impl IntoIterator<Item = OsString>) -> Result<Option<Self>, String> {
        let mut arguments = arguments.into_iter();
        let mut path = None;
        let mut limit = 12;
        let mut passes = 3;
        let mut workers = vec![1, 2, 3];
        let mut scales = vec![1, 2, 4];
        let mut reverse = false;
        let mut positional_only = false;
        while let Some(argument) = arguments.next() {
            match argument.to_str().filter(|_| !positional_only) {
                Some("-h" | "--help") => return Ok(None),
                Some("--") => positional_only = true,
                Some("--reverse") => reverse = true,
                Some(option @ ("--limit" | "--passes" | "--workers" | "--scales")) => {
                    let value = arguments
                        .next()
                        .ok_or_else(|| format!("{option} needs a value"))?
                        .into_string()
                        .map_err(|_| format!("{option} needs a UTF-8 value"))?;
                    match option {
                        "--limit" => limit = bounded_number(&value, option, 500)?,
                        "--passes" => passes = bounded_number(&value, option, 100)?,
                        "--workers" => workers = parse_list(&value, option, &[1, 2, 3])?,
                        "--scales" => scales = parse_list(&value, option, &[1, 2, 4])?,
                        _ => unreachable!(),
                    }
                }
                Some(option) if option.starts_with('-') => {
                    return Err(format!("unknown option: {option}"));
                }
                _ => {
                    if path.replace(PathBuf::from(argument)).is_some() {
                        return Err("provide exactly one folder or ARW file".into());
                    }
                }
            }
        }
        if limit * passes > MAX_SAMPLES {
            return Err(format!(
                "--limit times --passes must not exceed {MAX_SAMPLES}"
            ));
        }
        Ok(Some(Self {
            path: path.ok_or("provide a folder or ARW file; use --help for usage")?,
            limit,
            passes,
            workers,
            scales,
            reverse,
        }))
    }

    fn combinations(&self) -> Vec<(usize, u32)> {
        let mut combinations: Vec<_> = self
            .scales
            .iter()
            .flat_map(|&scale| {
                self.workers
                    .iter()
                    .map(move |&workers| (workers, scale as u32))
            })
            .collect();
        if self.reverse {
            combinations.reverse();
        }
        combinations
    }
}

fn bounded_number(value: &str, option: &str, max: usize) -> Result<usize, String> {
    value
        .parse()
        .ok()
        .filter(|number| (1..=max).contains(number))
        .ok_or_else(|| format!("{option} must be an integer between 1 and {max}"))
}

fn parse_list(value: &str, option: &str, allowed: &[usize]) -> Result<Vec<usize>, String> {
    let mut result = Vec::new();
    for element in value.split(',') {
        let number = element
            .parse()
            .ok()
            .filter(|number| allowed.contains(number))
            .ok_or_else(|| format!("{option} must be a comma-separated subset of {allowed:?}"))?;
        if result.contains(&number) {
            return Err(format!("{option} contains duplicate value {number}"));
        }
        result.push(number);
    }
    Ok(result)
}

/// Account for allocations, including any trailing JPEG padding retained by Vec.
fn load_corpus(args: &Args) -> Result<(Corpus, usize), String> {
    let paths = scan_arw_paths(&args.path).map_err(|error| error.to_string())?;
    let mut corpus = Vec::new();
    let mut allocated = 0;
    for path in paths.iter().take(args.limit) {
        let mut reader =
            PreviewReader::open(path).map_err(|error| format!("{}: {error}", path.display()))?;
        let previews = reader
            .find_previews()
            .map_err(|error| format!("{}: {error}", path.display()))?;
        let mut selected = None;
        for preview in previews {
            // Never choose a smaller preview just to squeeze another image in.
            if preview.length > (MAX_CORPUS_BYTES - allocated) as u64 {
                return finish_corpus(corpus, allocated);
            }
            let mut bytes = Vec::new();
            match reader.read_preview_into(&preview, &mut bytes) {
                Ok(()) => {
                    if bytes.capacity() > MAX_CORPUS_BYTES - allocated {
                        return finish_corpus(corpus, allocated);
                    }
                    selected = Some(bytes);
                    break;
                }
                Err(error) => eprintln!("{}: skipping invalid candidate: {error}", path.display()),
            }
        }
        let bytes = selected.ok_or_else(|| format!("{}: no usable preview", path.display()))?;
        allocated += bytes.capacity();
        corpus.push(bytes);
    }
    finish_corpus(corpus, allocated)
}

fn finish_corpus(corpus: Vec<Vec<u8>>, bytes: usize) -> Result<(Corpus, usize), String> {
    if corpus.is_empty() {
        Err("no JPEGs loaded within the 256 MiB corpus budget".into())
    } else {
        Ok((Arc::new(corpus), bytes))
    }
}

#[derive(Default)]
struct WorkerSamples {
    durations: Vec<Duration>,
    decoded_bytes: u64,
}

struct Measurement {
    elapsed: Duration,
    samples: WorkerSamples,
}

fn measure(
    corpus: &Corpus,
    workers: usize,
    scale: u32,
    passes: usize,
) -> Result<Measurement, String> {
    let total = corpus
        .len()
        .checked_mul(passes)
        .filter(|total| (1..=MAX_SAMPLES).contains(total))
        .ok_or("timed sample count must be between 1 and 10000")?;
    if !(1..=3).contains(&workers) || !matches!(scale, 1 | 2 | 4) {
        return Err("workers must be 1, 2, or 3 and scale must be 1, 2, or 4".into());
    }
    let next = AtomicUsize::new(0);
    // The first barrier excludes setup and warmup. The second releases timed work.
    let ready = Barrier::new(workers + 1);
    let start = Barrier::new(workers + 1);
    std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(workers);
        for worker in 0..workers {
            let corpus = Arc::clone(corpus);
            let (next, ready, start) = (&next, &ready, &start);
            handles.push(scope.spawn(move || {
                let prepared = (|| {
                    let mut decoder = Decoder::with_scale(Backend::Turbo, scale)?;
                    decoder.decode(&corpus[worker % corpus.len()])?;
                    drop(std::hint::black_box(decoder.take_pixels()));
                    Ok::<_, String>(decoder)
                })();
                // A decoder error must still reach both barriers so peers cannot hang.
                ready.wait();
                start.wait();
                let mut decoder = prepared?;
                let mut samples = WorkerSamples::default();
                loop {
                    let job = next.fetch_add(1, Ordering::Relaxed);
                    if job >= total {
                        break;
                    }
                    let began = Instant::now();
                    let info = decoder.decode(&corpus[job % corpus.len()])?;
                    drop(std::hint::black_box(decoder.take_pixels()));
                    samples.durations.push(began.elapsed());
                    samples.decoded_bytes += info.bytes as u64;
                }
                Ok::<_, String>(samples)
            }));
        }
        ready.wait();
        let began = Instant::now();
        start.wait();
        let mut samples = WorkerSamples::default();
        let mut error = None;
        for handle in handles {
            match handle.join() {
                Ok(Ok(worker)) => {
                    samples.durations.extend(worker.durations);
                    samples.decoded_bytes += worker.decoded_bytes;
                }
                Ok(Err(message)) => error = Some(message),
                Err(_) => error = Some("decode worker panicked".into()),
            }
        }
        if let Some(error) = error {
            return Err(error);
        }
        if samples.durations.len() != total {
            return Err("workers did not complete the requested sample count".into());
        }
        Ok(Measurement {
            elapsed: began.elapsed(),
            samples,
        })
    })
}

fn percentile(sorted: &[Duration], percentile: usize) -> Duration {
    sorted[(sorted.len() * percentile).div_ceil(100).saturating_sub(1)]
}

fn run(args: Args) -> Result<(), String> {
    // Fail before touching any RAWs when TurboJPEG was omitted from the build.
    Decoder::new(Backend::Turbo)?;
    let (corpus, bytes) = load_corpus(&args)?;
    println!("Memory-only JPEG decode throughput (TurboJPEG, fresh RGBA allocation)");
    println!(
        "Corpus: {} naturally sorted files; {:.2} MiB compressed; {} passes",
        corpus.len(),
        bytes as f64 / 1_048_576.0,
        args.passes
    );
    if corpus.len() < args.limit {
        println!(
            "Requested limit {}, loaded {} within available files and the 256 MiB cap.",
            args.limit,
            corpus.len()
        );
    }
    println!("All workers are busy decoders; no foreground thread is reserved.");
    println!("Excludes disk, UI, GPU, and prefetch scheduling. This is not navigation latency.\n");
    println!("scale  workers  images/s   p50_ms   p95_ms  mean_RGBA_MiB  elapsed_s  samples");
    for (workers, scale) in args.combinations() {
        let mut result = measure(&corpus, workers, scale, args.passes)?;
        result.samples.durations.sort_unstable();
        let count = result.samples.durations.len();
        println!(
            "1/{scale:<4} {workers:>7} {:>9.2} {:>8.2} {:>8.2} {:>14.2} {:>10.3} {count:>8}",
            count as f64 / result.elapsed.as_secs_f64(),
            percentile(&result.samples.durations, 50).as_secs_f64() * 1000.0,
            percentile(&result.samples.durations, 95).as_secs_f64() * 1000.0,
            result.samples.decoded_bytes as f64 / count as f64 / 1_048_576.0,
            result.elapsed.as_secs_f64()
        );
    }
    Ok(())
}

fn main() -> ExitCode {
    let outcome = Args::parse(std::env::args_os().skip(1)).and_then(|args| match args {
        Some(args) => run(args),
        None => {
            println!("{HELP}");
            Ok(())
        }
    });
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("fastcull-decode-throughput-bench: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Option<Args>, String> {
        Args::parse(args.iter().map(OsString::from))
    }

    #[test]
    fn bounds_work_and_rejects_invalid_or_duplicate_combinations() {
        for args in [
            vec!["photos", "--limit", "0"],
            vec!["photos", "--limit", "501"],
            vec!["photos", "--passes", "101"],
            vec!["photos", "--limit", "500", "--passes", "100"],
            vec!["photos", "--workers", "0"],
            vec!["photos", "--workers", "1,1"],
            vec!["photos", "--workers", "4"],
            vec!["photos", "--scales", "3"],
            vec!["photos", "--scales", ""],
            vec!["photos", "--workers"],
            vec!["photos", "--unknown"],
            vec!["photos", "another-folder"],
        ] {
            assert!(parse(&args).is_err(), "{args:?}");
        }
        assert!(parse(&["photos", "--limit", "100", "--passes", "100"]).is_ok());
    }

    #[test]
    fn preserves_requested_combination_order_and_can_reverse_it() {
        let args = parse(&["photos", "--workers", "3,1", "--scales", "4,2"])
            .unwrap()
            .unwrap();
        assert_eq!(args.combinations(), [(3, 4), (1, 4), (3, 2), (1, 2)]);
        let reversed = Args {
            reverse: true,
            ..args
        };
        assert_eq!(reversed.combinations(), [(1, 2), (3, 2), (1, 4), (3, 4)]);
        assert_eq!(
            parse(&["--", "-photos"]).unwrap().unwrap().path,
            PathBuf::from("-photos")
        );
    }

    #[test]
    fn nearest_rank_percentiles_include_the_tail_and_single_sample() {
        let times: Vec<_> = (1..=20).map(Duration::from_millis).collect();
        assert_eq!(percentile(&times, 50), Duration::from_millis(10));
        assert_eq!(percentile(&times, 95), Duration::from_millis(19));
        assert_eq!(percentile(&times[..1], 95), Duration::from_millis(1));
    }

    #[cfg(feature = "turbo")]
    #[test]
    fn invalid_jpeg_fails_without_stranding_workers_at_the_start_barrier() {
        let corpus = Arc::new(vec![vec![0xff, 0xd8, 0xff, 0xd9]]);
        for workers in 1..=3 {
            assert!(measure(&corpus, workers, 1, 1).is_err());
        }
    }

    #[cfg(feature = "turbo")]
    #[test]
    fn workers_complete_each_decode_and_transfer_all_expected_pixels() {
        // Generated 8x8 gray JPEG: no photographic fixture or third-party content.
        let mut jpeg = vec![0xff, 0xd8, 0xff, 0xdb, 0, 67, 0];
        jpeg.extend_from_slice(&[1; 64]);
        jpeg.extend_from_slice(&[0xff, 0xc0, 0, 11, 8, 0, 8, 0, 8, 1, 1, 0x11, 0]);
        jpeg.extend_from_slice(&[0xff, 0xc4, 0, 38]);
        for table in [0, 0x10] {
            jpeg.extend_from_slice(&[table, 1]);
            jpeg.extend_from_slice(&[0; 15]);
            jpeg.push(0);
        }
        jpeg.extend_from_slice(&[0xff, 0xda, 0, 8, 1, 1, 0, 0, 63, 0, 0x3f, 0xff, 0xd9]);
        let corpus = Arc::new(vec![jpeg.clone(), jpeg]);
        for workers in 1..=3 {
            for scale in [1, 2, 4] {
                let result = measure(&corpus, workers, scale, 3).unwrap();
                assert_eq!(result.samples.durations.len(), 6);
                assert_eq!(
                    result.samples.decoded_bytes,
                    6 * 256 / u64::from(scale * scale)
                );
            }
        }
        assert!(measure(&Arc::new(Vec::new()), 1, 1, 1).is_err());
        assert!(measure(&corpus, 1, 1, usize::MAX).is_err());
    }
}
