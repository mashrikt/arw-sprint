use fastcull::arw::PreviewReader;
use fastcull::browser::directory::scan_arw_paths;
use fastcull::image::decoder::{Backend, Decoder};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

const DEFAULT_LIMIT: usize = 500;
const MAX_LIMIT: usize = 100_000;
const MAX_PASSES: usize = 100;
const MAX_SAMPLES: usize = 1_000_000;
const MAX_PRINTED_ERRORS: usize = 20;

const HELP: &str = "fastcull-bench <folder-or-file.ARW> [options]

Benchmark embedded JPEG discovery, reading, and optional decoding.

  --limit N       First N naturally sorted files (default 500; max 100000)
  --passes N      Passes per order (default 1; max 100)
  --order ORDER   sequential | random | both (default both)
  --decoder NAME  none | turbo | zune | both (default none)
  --scale N       JPEG decode scale 1/N: 1, 2, 4, or 8 (default 1)
  --seed N        Unsigned decimal shuffle seed (default 24301)
  -h, --help      Show this help

JPEG decoding requires the corresponding cargo feature: turbo or zune.
--decoder both requires both features and uses one fixed file list for both.
Reduced scales require --decoder turbo; other modes use full resolution.
Set FASTCULL_LOG=1 for per-file timings and bytes requested from the file.
At most 1000000 file attempts are allowed across all passes, orders, and decoders.
OS disk caches are uncontrolled. Results must not be called cold-disk timings.
This measures a serial pipeline, not GUI or prefetch navigation latency.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Order {
    Sequential,
    Random,
    Both,
}

impl Order {
    fn name(self) -> &'static str {
        match self {
            Self::Sequential => "Sequential",
            Self::Random => "Random",
            Self::Both => "Both",
        }
    }
}

#[derive(Debug)]
struct Args {
    path: PathBuf,
    limit: usize,
    passes: usize,
    order: Order,
    decoder: String,
    scale: u32,
    seed: u64,
}

impl Args {
    fn parse(arguments: impl IntoIterator<Item = OsString>) -> Result<Option<Self>, String> {
        let mut arguments = arguments.into_iter();
        let mut path = None;
        let mut limit = DEFAULT_LIMIT;
        let mut passes = 1;
        let mut order = Order::Both;
        let mut decoder = String::from("none");
        let mut scale = 1;
        let mut seed = 24301;
        let mut positional_only = false;
        while let Some(argument) = arguments.next() {
            if positional_only {
                set_path(&mut path, argument)?;
                continue;
            }
            match argument.to_str() {
                Some("--") => positional_only = true,
                Some("-h" | "--help") => return Ok(None),
                Some("--limit") => {
                    limit = parse_bounded(
                        &next_value(&mut arguments, "--limit")?,
                        "--limit",
                        MAX_LIMIT,
                    )?;
                }
                Some("--passes") => {
                    passes = parse_bounded(
                        &next_value(&mut arguments, "--passes")?,
                        "--passes",
                        MAX_PASSES,
                    )?;
                }
                Some("--seed") => {
                    seed = next_value(&mut arguments, "--seed")?
                        .parse()
                        .map_err(|_| "--seed must be an unsigned decimal integer".to_string())?;
                }
                Some("--order") => {
                    order = match next_value(&mut arguments, "--order")?.as_str() {
                        "sequential" => Order::Sequential,
                        "random" => Order::Random,
                        "both" => Order::Both,
                        _ => return Err("--order must be sequential, random, or both".into()),
                    };
                }
                Some("--decoder") => {
                    decoder = next_value(&mut arguments, "--decoder")?;
                    if !matches!(decoder.as_str(), "none" | "turbo" | "zune" | "both") {
                        return Err("--decoder must be none, turbo, zune, or both".into());
                    }
                }
                Some("--scale") => {
                    scale = next_value(&mut arguments, "--scale")?
                        .parse::<u32>()
                        .ok()
                        .filter(|scale| matches!(scale, 1 | 2 | 4 | 8))
                        .ok_or_else(|| "--scale must be 1, 2, 4, or 8".to_string())?;
                }
                Some(value) if value.starts_with('-') => {
                    return Err(format!("unknown option: {value}"))
                }
                _ => set_path(&mut path, argument)?,
            }
        }
        if scale != 1 && decoder != "turbo" {
            return Err("--scale greater than 1 requires --decoder turbo".into());
        }
        Ok(Some(Self {
            path: path
                .ok_or_else(|| "provide a folder or ARW file; use --help for usage".to_string())?,
            limit,
            passes,
            order,
            decoder,
            scale,
            seed,
        }))
    }

    fn backends(&self) -> Result<Vec<Option<Backend>>, String> {
        match self.decoder.as_str() {
            "none" => Ok(vec![None]),
            "both" => Ok(vec![Some(Backend::Turbo), Some(Backend::Zune)]),
            decoder => Ok(vec![Some(Backend::parse(decoder)?)]),
        }
    }
}

fn set_path(path: &mut Option<PathBuf>, argument: OsString) -> Result<(), String> {
    if path.is_some() {
        return Err("provide exactly one folder or ARW file".into());
    }
    *path = Some(argument.into());
    Ok(())
}

fn next_value(
    arguments: &mut impl Iterator<Item = OsString>,
    option: &str,
) -> Result<String, String> {
    arguments
        .next()
        .ok_or_else(|| format!("{option} requires a value"))?
        .into_string()
        .map_err(|_| format!("{option} requires a UTF-8 value"))
}

fn parse_bounded(value: &str, option: &str, maximum: usize) -> Result<usize, String> {
    value
        .parse::<usize>()
        .ok()
        .filter(|value| (1..=maximum).contains(value))
        .ok_or_else(|| format!("{option} must be between 1 and {maximum}"))
}

#[derive(Debug)]
struct Sample {
    open_ms: f64,
    discovery_ms: f64,
    jpeg_read_ms: f64,
    decode_ms: f64,
    total_ms: f64,
}

#[derive(Default)]
struct IoTotals {
    bytes: u64,
    calls: u64,
}

fn milliseconds(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

fn measure_file(
    path: &Path,
    jpeg: &mut Vec<u8>,
    decoder: &mut Option<Decoder>,
    io: &mut IoTotals,
    log: bool,
) -> Result<Sample, String> {
    let total_start = Instant::now();
    let mut reader = PreviewReader::open(path).map_err(|error| format!("open: {error}"))?;
    let open_ms = milliseconds(total_start.elapsed());
    let discovery_start = Instant::now();
    let preview_result = reader.find_previews();
    let discovery_ms = milliseconds(discovery_start.elapsed());
    let previews = match preview_result {
        Ok(previews) => previews,
        Err(error) => {
            add_io(&reader, io);
            if log {
                log_problems(path, &reader, &[]);
            }
            return Err(format!("preview discovery: {error}"));
        }
    };
    let read_start = Instant::now();
    let mut selected = None;
    let mut read_errors = Vec::new();
    for preview in previews {
        match reader.read_preview_into(&preview, jpeg) {
            Ok(()) => {
                selected = Some(preview);
                break;
            }
            Err(error) => read_errors.push(format!("candidate at {}: {error}", preview.offset)),
        }
    }
    let jpeg_read_ms = milliseconds(read_start.elapsed());
    add_io(&reader, io);
    let preview = selected.ok_or_else(|| {
        if log {
            log_problems(path, &reader, &read_errors);
        }
        format!(
            "JPEG read: all {} candidates failed; {}",
            read_errors.len(),
            read_errors
                .last()
                .map(String::as_str)
                .unwrap_or("no candidates")
        )
    })?;

    let decode_start = Instant::now();
    let decoded = decoder
        .as_mut()
        .map(|decoder| {
            let info = decoder.decode(jpeg)?;
            std::hint::black_box(decoder.pixels());
            Ok::<_, String>(info)
        })
        .transpose()
        .map_err(|error| {
            if log {
                log_problems(path, &reader, &read_errors);
            }
            format!("JPEG decode: {error}")
        })?;
    let decode_ms = if decoded.is_some() {
        milliseconds(decode_start.elapsed())
    } else {
        0.0
    };
    let total_ms = milliseconds(total_start.elapsed());
    if log {
        log_problems(path, &reader, &read_errors);
        let stats = reader.io_stats();
        let dimensions = decoded
            .map(|decoded| {
                format!(
                    "{}x{} ({} decoded bytes)",
                    decoded.width, decoded.height, decoded.bytes
                )
            })
            .unwrap_or_else(|| match (preview.width, preview.height) {
                (Some(width), Some(height)) => format!("{width}x{height}"),
                _ => "unknown dimensions".to_string(),
            });
        eprintln!(
            "{}\n  open: {:.3} ms; parse: {:.3} ms; jpeg read: {:.3} ms; decode: {:.3} ms; total: {:.3} ms\n  preview: offset={}, length={}, {}; file reads: {} bytes in {} calls; fallback candidates skipped: {}",
            path.display(), open_ms, discovery_ms, jpeg_read_ms, decode_ms, total_ms,
            preview.offset, preview.length, dimensions, stats.bytes_read, stats.read_calls, read_errors.len(),
        );
    }
    Ok(Sample {
        open_ms,
        discovery_ms,
        jpeg_read_ms,
        decode_ms,
        total_ms,
    })
}

fn log_problems(path: &Path, reader: &PreviewReader, read_errors: &[String]) {
    for warning in reader.warnings().iter().chain(read_errors) {
        eprintln!("{}: {warning}", path.display());
    }
}

fn add_io(reader: &PreviewReader, totals: &mut IoTotals) {
    let stats = reader.io_stats();
    totals.bytes = totals.bytes.saturating_add(stats.bytes_read);
    totals.calls = totals.calls.saturating_add(stats.read_calls);
}

/// Nearest-rank percentile; measured samples are sorted only for presentation.
fn percentile(sorted: &[f64], percent: usize) -> Option<f64> {
    if sorted.is_empty() {
        return None;
    }
    let rank = sorted.len().saturating_mul(percent.min(100)).div_ceil(100);
    sorted.get(rank.saturating_sub(1)).copied()
}

fn print_metric(samples: &[Sample], name: &str, value: impl Fn(&Sample) -> f64) {
    let mut values: Vec<f64> = samples.iter().map(value).collect();
    values.sort_unstable_by(f64::total_cmp);
    if let (Some(p50), Some(p95), Some(p99)) = (
        percentile(&values, 50),
        percentile(&values, 95),
        percentile(&values, 99),
    ) {
        println!("{name:<22} {p50:>10.3} {p95:>10.3} {p99:>10.3}");
    }
}

// SplitMix64 gives reproducible order with no RNG dependency. Rejection sampling
// avoids modulo bias in Fisher-Yates. This RNG is not used for security.
struct ShuffleRng(u64);

impl ShuffleRng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e3779b97f4a7c15);
        let mut value = self.0;
        value = (value ^ (value >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94d049bb133111eb);
        value ^ (value >> 31)
    }

    fn below(&mut self, bound: u64) -> u64 {
        let threshold = bound.wrapping_neg() % bound;
        loop {
            let value = self.next();
            if value >= threshold {
                return value % bound;
            }
        }
    }
}

fn shuffle(indices: &mut [usize], rng: &mut ShuffleRng) {
    for current in (1..indices.len()).rev() {
        let other = rng.below((current + 1) as u64) as usize;
        indices.swap(current, other);
    }
}

fn run(args: Args) -> Result<(), String> {
    let backends = args.backends()?;
    for backend in backends.iter().flatten() {
        if !Backend::available().contains(backend) {
            return Err(format!(
                "JPEG backend {} is disabled; rebuild with --features {}",
                backend.name(),
                if args.decoder == "both" {
                    "turbo,zune"
                } else {
                    backend.name()
                }
            ));
        }
    }
    let scan_start = Instant::now();
    let mut paths =
        scan_arw_paths(&args.path).map_err(|error| format!("{}: {error}", args.path.display()))?;
    let scan_ms = milliseconds(scan_start.elapsed());
    let found = paths.len();
    paths.truncate(args.limit);
    if paths.is_empty() {
        return Err("no ARW files found (directory scanning is nonrecursive)".into());
    }
    let orders: &[Order] = match args.order {
        Order::Sequential => &[Order::Sequential],
        Order::Random => &[Order::Random],
        Order::Both => &[Order::Sequential, Order::Random],
    };
    let attempts_per_order = paths
        .len()
        .checked_mul(args.passes)
        .ok_or("attempt count overflow")?;
    let planned = attempts_per_order
        .checked_mul(orders.len())
        .and_then(|attempts| attempts.checked_mul(backends.len()))
        .ok_or("attempt count overflow")?;
    if planned > MAX_SAMPLES {
        return Err(format!(
            "{planned} planned attempts exceeds {MAX_SAMPLES}; lower --limit or --passes"
        ));
    }
    let log =
        std::env::var_os("FASTCULL_LOG").is_some_and(|value| !value.is_empty() && value != "0");

    println!("FastCull embedded-preview benchmark");
    println!(
        "Target: {}-{}; available logical CPUs: {}",
        std::env::consts::ARCH,
        std::env::consts::OS,
        std::thread::available_parallelism()
            .map(|value| value.get())
            .unwrap_or(1)
    );
    println!(
        "Files selected: {} / {}; passes per order: {}; decoder: {}",
        paths.len(),
        found,
        args.passes,
        args.decoder
    );
    println!("JPEG decode scale: 1/{}", args.scale);
    println!(
        "Directory enumeration + natural sort: {scan_ms:.3} ms; shuffle seed: {}",
        args.seed
    );
    println!("OS disk cache is uncontrolled; later passes/orders can benefit from earlier reads.");
    if args.decoder == "both" {
        println!("Decoder order: turbo, then zune. Both use the same selected file list and shuffle sequence; OS cache warming can favor zune.");
    }
    println!("Serial pipeline only. Total includes file open, discovery, JPEG read, and enabled decoding.");
    println!("Percentiles include successful attempts; no application cache or GPU is used.");

    let mut successful = 0;
    for backend in backends {
        let backend_name = backend.map(Backend::name).unwrap_or("none");
        let mut decoder = backend
            .map(|backend| Decoder::with_scale(backend, args.scale))
            .transpose()?;
        println!("\nDecoder: {backend_name}");
        let mut jpeg = Vec::new();
        // Reset the seed per backend so random orders are identical for comparison.
        let mut rng = ShuffleRng(args.seed);
        for &order in orders {
            let order_start = Instant::now();
            let mut samples = Vec::with_capacity(attempts_per_order);
            let mut failures = 0;
            let mut io = IoTotals::default();
            let mut indices: Vec<usize> = (0..paths.len()).collect();
            for _pass in 0..args.passes {
                if order == Order::Random {
                    // Reset before each shuffle so each pass depends only on the RNG.
                    for (index, value) in indices.iter_mut().enumerate() {
                        *value = index;
                    }
                    shuffle(&mut indices, &mut rng);
                }
                for &index in &indices {
                    match measure_file(&paths[index], &mut jpeg, &mut decoder, &mut io, log) {
                        Ok(sample) => samples.push(sample),
                        Err(error) => {
                            failures += 1;
                            if log || failures <= MAX_PRINTED_ERRORS {
                                eprintln!("{}: {error}", paths[index].display());
                            }
                        }
                    }
                }
            }
            let elapsed = order_start.elapsed();
            successful += samples.len();
            println!("\n{} navigation order", order.name());
            println!(
                "Attempted: {attempts_per_order}; succeeded: {}; failed: {failures}",
                samples.len()
            );
            if failures > MAX_PRINTED_ERRORS && !log {
                println!(
                    "Additional error messages suppressed: {} (FASTCULL_LOG=1 shows all)",
                    failures - MAX_PRINTED_ERRORS
                );
            }
            if !samples.is_empty() {
                println!(
                    "{:22} {:>10} {:>10} {:>10}",
                    "Milliseconds", "p50", "p95", "p99"
                );
                print_metric(&samples, "File open", |sample| sample.open_ms);
                print_metric(&samples, "Preview discovery", |sample| sample.discovery_ms);
                print_metric(&samples, "JPEG read/extraction", |sample| {
                    sample.jpeg_read_ms
                });
                if decoder.is_some() {
                    print_metric(&samples, "JPEG decoding", |sample| sample.decode_ms);
                } else {
                    println!("JPEG decoding          disabled");
                }
                print_metric(&samples, "Total pipeline", |sample| sample.total_ms);
            }
            println!(
                "File bytes requested: {}; read calls: {} (includes failed attempts)",
                io.bytes, io.calls
            );
            println!(
                "Elapsed: {:.3} s; successful files/s: {:.2}; retained JPEG capacity: {} bytes",
                elapsed.as_secs_f64(),
                samples.len() as f64 / elapsed.as_secs_f64().max(f64::EPSILON),
                jpeg.capacity()
            );
        }
    }
    if successful == 0 {
        return Err("no usable previews were successfully processed".into());
    }
    Ok(())
}

fn main() -> ExitCode {
    match Args::parse(std::env::args_os().skip(1)) {
        Ok(None) => {
            println!("{HELP}");
            ExitCode::SUCCESS
        }
        Ok(Some(args)) => match run(args) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("fastcull-bench: {error}");
                ExitCode::FAILURE
            }
        },
        Err(error) => {
            eprintln!("fastcull-bench: {error}\nUse --help for usage.");
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles_use_nearest_rank_and_handle_small_samples() {
        assert_eq!(percentile(&[], 50), None);
        assert_eq!(percentile(&[7.0], 99), Some(7.0));
        let samples: Vec<f64> = (1..=100).map(f64::from).collect();
        assert_eq!(percentile(&samples, 50), Some(50.0));
        assert_eq!(percentile(&samples, 95), Some(95.0));
        assert_eq!(percentile(&samples, 99), Some(99.0));
        assert_eq!(percentile(&samples, 100), Some(100.0));
    }

    #[test]
    fn shuffle_is_deterministic_and_preserves_all_indices() {
        let original: Vec<usize> = (0..100).collect();
        let mut a = original.clone();
        let mut b = original.clone();
        shuffle(&mut a, &mut ShuffleRng(42));
        shuffle(&mut b, &mut ShuffleRng(42));
        assert_eq!(a, b);
        assert_ne!(a, original);
        a.sort_unstable();
        assert_eq!(a, original);
        shuffle(&mut [], &mut ShuffleRng(0));
        shuffle(&mut [0], &mut ShuffleRng(0));
    }

    #[test]
    fn args_reject_unbounded_work_and_unknown_options() {
        for arguments in [
            vec!["photos", "--limit", "0"],
            vec!["photos", "--limit", "100001"],
            vec!["photos", "--passes", "101"],
            vec!["photos", "--order", "invalid"],
            vec!["photos", "--decoder", "invalid"],
            vec!["photos", "--unknown"],
        ] {
            assert!(Args::parse(arguments.into_iter().map(OsString::from)).is_err());
        }
    }

    #[test]
    fn default_args_and_explicit_options() {
        let defaults = Args::parse([OsString::from("photos")]).unwrap().unwrap();
        assert_eq!(defaults.limit, DEFAULT_LIMIT);
        assert_eq!(defaults.order, Order::Both);
        assert_eq!(defaults.decoder, "none");
        assert_eq!(defaults.scale, 1);
        let arguments = [
            "--passes", "2", "--limit", "12", "--seed", "0", "--order", "random", "--", "-photos",
        ];
        let args = Args::parse(arguments.into_iter().map(OsString::from))
            .unwrap()
            .unwrap();
        assert_eq!(args.path, PathBuf::from("-photos"));
        assert_eq!(args.order, Order::Random);
        assert_eq!(args.passes, 2);
        assert_eq!(args.seed, 0);
    }

    #[test]
    fn cli_both_decoders_selects_turbo_then_zune_with_one_path() {
        let arguments = [
            "photos",
            "--decoder",
            "both",
            "--order",
            "both",
            "--limit",
            "200",
        ];
        let args = Args::parse(arguments.into_iter().map(OsString::from))
            .unwrap()
            .unwrap();
        assert_eq!(args.path, PathBuf::from("photos"));
        assert_eq!(args.order, Order::Both);
        assert_eq!(args.limit, 200);
        assert_eq!(args.scale, 1);
        assert_eq!(
            args.backends().unwrap(),
            [Some(Backend::Turbo), Some(Backend::Zune)]
        );
    }

    #[test]
    fn cli_scale_accepts_only_supported_turbo_denominators() {
        for scale in ["1", "2", "4", "8"] {
            let arguments = ["photos", "--scale", scale, "--decoder", "turbo"];
            let args = Args::parse(arguments.into_iter().map(OsString::from))
                .unwrap()
                .unwrap();
            assert_eq!(args.scale, scale.parse::<u32>().unwrap());
        }
        for scale in ["0", "3", "16", "-1", "4294967296", "invalid"] {
            let arguments = ["photos", "--decoder", "turbo", "--scale", scale];
            assert!(Args::parse(arguments.into_iter().map(OsString::from)).is_err());
        }
        for decoder in ["none", "zune", "both"] {
            let arguments = ["photos", "--decoder", decoder, "--scale", "2"];
            assert!(Args::parse(arguments.into_iter().map(OsString::from)).is_err());
        }
    }
}
