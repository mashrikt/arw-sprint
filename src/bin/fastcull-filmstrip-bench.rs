//! Read-only filmstrip research. This deliberately does not model the GUI/GPU.
use fastcull::arw::{EmbeddedPreview, PreviewReader};
use fastcull::browser::directory::scan_arw_paths;
use fastcull::image::decoder::{Backend, Decoder};
use std::collections::HashMap;
use std::ffi::OsString;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Instant;

const THUMB_CACHE_BYTES: usize = 16 * 1024 * 1024;
const VISIBLE_RADIUS: usize = 5;
const MAX_ROWS: usize = 200_000;
const HELP: &str = "fastcull-filmstrip-bench <folder-or-file.ARW> [options]

Read-only embedded-preview and CPU/I/O-contention research; requires --features turbo.
Raw TSV is written to stdout, summaries and limitations to stderr.

  --mode MODE        micro | contention | both (default micro)
  --strategy NAME    tiny | medium | medium-eighth | large | all (default all)
                     Comma-separated names are also accepted.
  --order ORDER      sequential | random (default sequential)
  --limit N          First N naturally sorted files (default 64; max 10000)
  --spread           Select evenly across the sorted folder instead of first N
  --rounds N         Repetitions; four contention passes/round (default 1; max 8)
  --seed N           Seed for matched random orders (default 24301)
  -h, --help         Show this help

tiny = smallest embedded JPEG, full decode.
medium = smallest JPEG with long edge >=1600 pixels, decode at 1/4.
medium-eighth = same selection as medium, decode at 1/8.
large = largest JPEG, decode at 1/8. Missing medium falls back to largest.
Actual selected dimensions are recorded; no three-preview layout is assumed.

Contention: A is full-size main decode only; B adds one thumbnail worker.
Rounds alternate A B B A / B A A B, each on the SAME file order. Thumbnail work follows the
latest visible window (current +/-5), with a 16 MiB CPU cache and no task backlog.
This conservatively permits thumbnail CPU/I/O work during main-image decoding.
It does NOT simulate production foreground/prefetch scheduling, GPU upload,
512 MiB shared cache pressure, displayed latency, or idle CPU/thermal behavior.
OS disk caches and other processes are uncontrolled. No cold-disk claims.
All selected-file errors are fatal. No RAW sensor decoding or sidecar writes.";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Strategy {
    Tiny,
    Medium,
    MediumEighth,
    Large,
}
impl Strategy {
    fn name(self) -> &'static str {
        match self {
            Self::Tiny => "tiny",
            Self::Medium => "medium",
            Self::MediumEighth => "medium-eighth",
            Self::Large => "large",
        }
    }
    fn scale(self) -> u32 {
        match self {
            Self::Tiny => 1,
            Self::Medium => 4,
            Self::MediumEighth | Self::Large => 8,
        }
    }
    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "tiny" => Ok(Self::Tiny),
            "medium" => Ok(Self::Medium),
            "medium-eighth" => Ok(Self::MediumEighth),
            "large" => Ok(Self::Large),
            _ => Err(format!("unknown strategy {value:?}")),
        }
    }
}

#[derive(Debug)]
struct Args {
    path: PathBuf,
    mode: String,
    strategies: Vec<Strategy>,
    order: String,
    limit: usize,
    rounds: usize,
    seed: u64,
    spread: bool,
}
impl Args {
    fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Option<Self>, String> {
        let mut args = args.into_iter();
        let mut result = Self {
            path: PathBuf::new(),
            mode: "micro".into(),
            strategies: vec![
                Strategy::Tiny,
                Strategy::Medium,
                Strategy::MediumEighth,
                Strategy::Large,
            ],
            order: "sequential".into(),
            limit: 64,
            rounds: 1,
            seed: 24301,
            spread: false,
        };
        let mut path = None;
        let mut positional = false;
        while let Some(arg) = args.next() {
            if !positional && arg == "--" {
                positional = true;
                continue;
            }
            if !positional && matches!(arg.to_str(), Some("-h" | "--help")) {
                return Ok(None);
            }
            if !positional && arg == "--spread" {
                result.spread = true;
                continue;
            }
            if !positional && arg.to_string_lossy().starts_with('-') {
                let flag = arg.to_str().ok_or("option must be UTF-8")?;
                if !matches!(
                    flag,
                    "--mode" | "--strategy" | "--order" | "--limit" | "--rounds" | "--seed"
                ) {
                    return Err(format!("unknown option {flag:?}"));
                }
                let value = args
                    .next()
                    .ok_or_else(|| format!("missing value for {flag}"))?
                    .into_string()
                    .map_err(|_| format!("{flag} value must be UTF-8"))?;
                match flag {
                    "--mode" if matches!(value.as_str(), "micro" | "contention" | "both") => {
                        result.mode = value
                    }
                    "--order" if matches!(value.as_str(), "sequential" | "random") => {
                        result.order = value
                    }
                    "--strategy" => {
                        if value != "all" {
                            result.strategies = value
                                .split(',')
                                .map(Strategy::parse)
                                .collect::<Result<_, _>>()?;
                            result.strategies.dedup();
                        }
                    }
                    "--limit" => result.limit = bounded(&value, flag, 10_000)?,
                    "--rounds" => result.rounds = bounded(&value, flag, 8)?,
                    "--seed" => result.seed = value.parse().map_err(|_| "--seed must be u64")?,
                    _ => return Err(format!("invalid value {value:?} for {flag}")),
                }
            } else if path.replace(PathBuf::from(arg)).is_some() {
                return Err("provide exactly one input path".into());
            }
        }
        result.path = path.ok_or("missing input path; use --help")?;
        // Each B pass can decode at most eleven thumbnails per request. Keep
        // even the worst-case finite result storage bounded before opening files.
        let rows_per_file = usize::from(result.mode != "contention")
            + usize::from(result.mode != "micro") * (4 + 2 * (VISIBLE_RADIUS * 2 + 1));
        let rows = result
            .limit
            .checked_mul(result.rounds)
            .and_then(|n| n.checked_mul(result.strategies.len()))
            .and_then(|n| n.checked_mul(rows_per_file))
            .ok_or("sample count overflow")?;
        if rows > MAX_ROWS {
            return Err(format!(
                "worst-case samples {rows} exceed {MAX_ROWS}; reduce limit/rounds/strategies"
            ));
        }
        Ok(Some(result))
    }
}

fn bounded(value: &str, flag: &str, max: usize) -> Result<usize, String> {
    value
        .parse::<usize>()
        .ok()
        .filter(|n| (1..=max).contains(n))
        .ok_or_else(|| format!("{flag} must be between 1 and {max}"))
}

fn pixels(preview: &EmbeddedPreview) -> u64 {
    u64::from(preview.width.unwrap_or(0)) * u64::from(preview.height.unwrap_or(0))
}

fn select_preview(
    previews: &[EmbeddedPreview],
    strategy: Strategy,
) -> Result<&EmbeddedPreview, String> {
    let key = |p: &&EmbeddedPreview| (pixels(p), p.length);
    let largest = previews
        .iter()
        .filter(|p| pixels(p) > 0)
        .max_by_key(key)
        .ok_or("no dimension-verified embedded JPEG")?;
    Ok(match strategy {
        Strategy::Tiny => previews
            .iter()
            .filter(|p| pixels(p) > 0)
            .min_by_key(key)
            .unwrap_or(largest),
        Strategy::Medium | Strategy::MediumEighth => previews
            .iter()
            .filter(|p| p.width.unwrap_or(0).max(p.height.unwrap_or(0)) >= 1600)
            .min_by_key(key)
            .unwrap_or(largest),
        Strategy::Large => largest,
    })
}

#[derive(Clone, Debug, Default)]
struct Sample {
    index: usize,
    request: u64,
    source_width: u32,
    source_height: u32,
    width: u32,
    height: u32,
    jpeg_bytes: u64,
    rgba_bytes: usize,
    file_bytes_read: u64,
    read_calls: u64,
    warnings: usize,
    open_ms: f64,
    discover_ms: f64,
    read_ms: f64,
    decode_ms: f64,
    total_ms: f64,
    request_ms: f64,
    discarded: bool,
}

fn elapsed(start: Instant) -> f64 {
    start.elapsed().as_secs_f64() * 1000.0
}

fn measure(
    path: &Path,
    index: usize,
    strategy: Strategy,
    decoder: &mut Decoder,
    jpeg: &mut Vec<u8>,
) -> Result<Sample, String> {
    let start = Instant::now();
    let mut sample = Sample {
        index,
        ..Sample::default()
    };
    let mut reader =
        PreviewReader::open(path).map_err(|e| format!("{}: open: {e}", path.display()))?;
    sample.open_ms = elapsed(start);
    let stage = Instant::now();
    let previews = reader.find_previews().map_err(|e| {
        format!(
            "{}: discovery: {e}; warnings={:?}",
            path.display(),
            reader.warnings()
        )
    })?;
    let preview = select_preview(&previews, strategy)?;
    sample.discover_ms = elapsed(stage);
    sample.source_width = preview.width.unwrap_or(0);
    sample.source_height = preview.height.unwrap_or(0);
    sample.jpeg_bytes = preview.length;
    sample.warnings = reader.warnings().len();
    let stage = Instant::now();
    reader
        .read_preview_into(preview, jpeg)
        .map_err(|e| format!("{}: selected JPEG read: {e}", path.display()))?;
    sample.read_ms = elapsed(stage);
    let io = reader.io_stats();
    sample.file_bytes_read = io.bytes_read;
    sample.read_calls = io.read_calls;
    let stage = Instant::now();
    let decoded = decoder
        .decode(jpeg)
        .map_err(|e| format!("{}: selected JPEG decode: {e}", path.display()))?;
    sample.decode_ms = elapsed(stage);
    sample.width = decoded.width;
    sample.height = decoded.height;
    sample.rgba_bytes = decoded.bytes;
    sample.total_ms = elapsed(start);
    sample.request_ms = sample.total_ms;
    Ok(sample)
}

// SplitMix64 + Fisher-Yates, with rejection sampling and no RNG dependency.
fn shuffled_order(len: usize, mut seed: u64) -> Vec<usize> {
    let mut result: Vec<_> = (0..len).collect();
    for i in (1..len).rev() {
        let bound = (i + 1) as u64;
        let threshold = bound.wrapping_neg() % bound;
        let value = loop {
            seed = seed.wrapping_add(0x9e3779b97f4a7c15);
            let mut value = seed;
            value = (value ^ (value >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
            value = (value ^ (value >> 27)).wrapping_mul(0x94d049bb133111eb);
            value ^= value >> 31;
            if value >= threshold {
                break value;
            }
        };
        result.swap(i, (value % bound) as usize);
    }
    result
}

fn visible_window(index: usize, len: usize) -> Vec<usize> {
    let mut result = vec![index];
    for distance in 1..=VISIBLE_RADIUS {
        if let Some(next) = index.checked_add(distance).filter(|n| *n < len) {
            result.push(next);
        }
        if let Some(previous) = index.checked_sub(distance) {
            result.push(previous);
        }
    }
    result
}

fn spread_indices(len: usize, limit: usize) -> Vec<usize> {
    let count = limit.min(len);
    match count {
        0 => Vec::new(),
        1 => vec![0],
        _ => (0..count)
            .map(|i| ((i as u128 * (len - 1) as u128) / (count - 1) as u128) as usize)
            .collect(),
    }
}

#[derive(Default)]
struct Latest {
    index: AtomicUsize,
    version: AtomicU64,
    stop: AtomicBool,
}
#[derive(Default)]
struct Thumbnails {
    samples: Vec<Sample>,
    cache_hits: usize,
    windows: usize,
    ready_windows: usize,
    peak_cache_bytes: usize,
    peak_rgba_bytes: usize,
    peak_jpeg_capacity: usize,
    budget_skips: usize,
}

fn thumbnail_worker(
    files: &[PathBuf],
    strategy: Strategy,
    latest: &Latest,
) -> Result<Thumbnails, String> {
    let mut decoder = Decoder::with_scale(Backend::Turbo, strategy.scale())?;
    let mut jpeg = Vec::new();
    let mut cache = HashMap::<usize, Vec<u8>>::new();
    let mut stats = Thumbnails::default();
    let mut last_version = 0;
    while !latest.stop.load(Ordering::Acquire) {
        let version = latest.version.load(Ordering::Acquire);
        if version == last_version {
            // unpark has a retained token, so a producer update between the
            // version check and park cannot lose a wakeup. Producer never waits.
            thread::park();
            continue;
        }
        last_version = version;
        let window = visible_window(latest.index.load(Ordering::Acquire), files.len());
        stats.windows += 1;
        cache.retain(|index, _| window.contains(index));
        let mut cache_bytes = cache.values().map(Vec::capacity).sum::<usize>();
        for index in window.iter().copied() {
            if latest.stop.load(Ordering::Acquire)
                || latest.version.load(Ordering::Acquire) != version
            {
                break;
            }
            if cache.contains_key(&index) {
                stats.cache_hits += 1;
                continue;
            }
            let mut sample = measure(&files[index], index, strategy, &mut decoder, &mut jpeg)?;
            sample.request = version;
            let pixels = decoder.take_pixels();
            stats.peak_rgba_bytes = stats.peak_rgba_bytes.max(cache_bytes + pixels.capacity());
            stats.peak_jpeg_capacity = stats.peak_jpeg_capacity.max(jpeg.capacity());
            sample.discarded = latest.stop.load(Ordering::Acquire)
                || latest.version.load(Ordering::Acquire) != version;
            if !sample.discarded {
                if pixels.capacity() <= THUMB_CACHE_BYTES.saturating_sub(cache_bytes) {
                    cache_bytes += pixels.capacity();
                    cache.insert(index, pixels);
                    stats.peak_cache_bytes = stats.peak_cache_bytes.max(cache_bytes);
                } else {
                    stats.budget_skips += 1;
                    // Preserve the nearest thumbnails already cached; do not
                    // churn through more distant entries when the cache fills.
                    stats.samples.push(sample);
                    break;
                }
            }
            stats.samples.push(sample);
        }
        if !latest.stop.load(Ordering::Acquire)
            && latest.version.load(Ordering::Acquire) == version
            && window.iter().all(|index| cache.contains_key(index))
        {
            stats.ready_windows += 1;
        }
    }
    Ok(stats)
}

struct Pass {
    main: Vec<Sample>,
    thumbs: Thumbnails,
    foreground_ms: f64,
    elapsed_ms: f64,
}

fn run_pass(
    files: &[PathBuf],
    order: &[usize],
    strategy: Strategy,
    concurrent: bool,
) -> Result<Pass, String> {
    let start = Instant::now();
    let latest = Arc::new(Latest::default());
    thread::scope(|scope| {
        let worker = if concurrent {
            let latest = Arc::clone(&latest);
            Some(scope.spawn(move || thumbnail_worker(files, strategy, &latest)))
        } else {
            None
        };
        let result = (|| {
            let mut decoder = Decoder::new(Backend::Turbo)?;
            let mut jpeg = Vec::new();
            let mut samples = Vec::with_capacity(order.len());
            let foreground = Instant::now();
            for (request, &index) in order.iter().enumerate() {
                let requested = Instant::now();
                if let Some(worker) = &worker {
                    latest.index.store(index, Ordering::Release);
                    latest.version.fetch_add(1, Ordering::Release);
                    worker.thread().unpark();
                }
                // Full-resolution baseline always selects the largest preview.
                let mut sample = measure(
                    &files[index],
                    index,
                    Strategy::Large,
                    &mut decoder,
                    &mut jpeg,
                )?;
                sample.request = request as u64 + 1;
                sample.request_ms = elapsed(requested);
                samples.push(sample);
            }
            Ok::<_, String>((samples, elapsed(foreground)))
        })();
        latest.stop.store(true, Ordering::Release);
        let thumbs = if let Some(worker) = worker {
            worker.thread().unpark();
            worker
                .join()
                .map_err(|_| "thumbnail worker panicked".to_string())?
        } else {
            Ok(Thumbnails::default())
        };
        let (main, foreground_ms) = result?;
        Ok(Pass {
            main,
            thumbs: thumbs?,
            foreground_ms,
            elapsed_ms: elapsed(start),
        })
    })
}

fn percentile(values: &[f64], percent: usize) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut values = values.to_vec();
    values.sort_unstable_by(f64::total_cmp);
    let rank = values
        .len()
        .saturating_mul(percent.clamp(1, 100))
        .div_ceil(100);
    values[rank - 1]
}

fn metric(name: &str, values: &[f64]) {
    let mean = values.iter().sum::<f64>() / values.len().max(1) as f64;
    eprintln!(
        "  {name:<20} n={:<6} mean={:>9.3} p50={:>9.3} p95={:>9.3} p99={:>9.3} ms",
        values.len(),
        mean,
        percentile(values, 50),
        percentile(values, 95),
        percentile(values, 99)
    );
}

fn summarize(label: &str, samples: &[Sample]) {
    eprintln!("{label}");
    for (name, field) in [
        ("open", (|s: &Sample| s.open_ms) as fn(&Sample) -> f64),
        ("discovery", |s: &Sample| s.discover_ms),
        ("JPEG read", |s: &Sample| s.read_ms),
        ("decode", |s: &Sample| s.decode_ms),
        ("total pipeline", |s: &Sample| s.total_ms),
        ("request incl enqueue", |s: &Sample| s.request_ms),
    ] {
        metric(name, &samples.iter().map(field).collect::<Vec<_>>());
    }
    eprintln!(
        "  requested_file_bytes={} max_JPEG_bytes={} max_RGBA_bytes={} parser_warnings={}",
        samples.iter().map(|s| s.file_bytes_read).sum::<u64>(),
        samples.iter().map(|s| s.jpeg_bytes).max().unwrap_or(0),
        samples.iter().map(|s| s.rgba_bytes).max().unwrap_or(0),
        samples.iter().map(|s| s.warnings).sum::<usize>()
    );
}

struct Label<'a> {
    phase: &'a str,
    strategy: Strategy,
    order: &'a str,
    round: usize,
    block: usize,
    condition: &'a str,
    role: &'a str,
}

fn escaped(path: &Path) -> String {
    path.to_string_lossy()
        .replace('\\', "\\\\")
        .replace('\t', "\\t")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}
fn write_samples(
    out: &mut impl Write,
    files: &[PathBuf],
    label: &Label<'_>,
    samples: &[Sample],
) -> io::Result<()> {
    for s in samples {
        writeln!(out, "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{:.6}\t{:.6}\t{:.6}\t{:.6}\t{:.6}\t{:.6}",
            label.phase, label.strategy.name(), label.order, label.round, label.block, label.condition, label.role,
            s.request, s.index, escaped(&files[s.index]), s.discarded, s.source_width, s.source_height,
            s.width, s.height, s.jpeg_bytes, s.rgba_bytes, s.file_bytes_read, s.read_calls, s.warnings,
            s.open_ms, s.discover_ms, s.read_ms, s.decode_ms, s.total_ms, s.request_ms)?;
    }
    Ok(())
}

fn run(args: Args) -> Result<(), String> {
    if !Backend::available().contains(&Backend::Turbo) {
        return Err("requires --features turbo".into());
    }
    let mut files =
        scan_arw_paths(&args.path).map_err(|e| format!("scan {}: {e}", args.path.display()))?;
    let available_files = files.len();
    if args.spread {
        files = spread_indices(files.len(), args.limit)
            .into_iter()
            .map(|index| files[index].clone())
            .collect();
    } else {
        files.truncate(args.limit);
    }
    if files.is_empty() {
        return Err("no ARW files found".into());
    }
    eprintln!(
        "Filmstrip CPU/I/O research: files={} rounds={} order={} seed={} mode={}",
        files.len(),
        args.rounds,
        args.order,
        args.seed,
        args.mode
    );
    eprintln!("One fixed file-list snapshot. Inputs are read-only. OS caches and background processes are uncontrolled.");
    eprintln!("Selection: {} of {available_files} files, method={}; thumbnail windows follow the selected list.", files.len(), if args.spread { "evenly-spread" } else { "first-N" });
    eprintln!("No GUI/GPU, production prefetch, shared 512 MiB accounting, or navigation-latency claim. Thumbnail cache cap={} bytes; one extra in-flight image is measured separately.", THUMB_CACHE_BYTES);
    let mut output = io::BufWriter::new(io::stdout().lock());
    writeln!(output, "phase\tstrategy\torder\tround\tblock\tcondition\trole\trequest\tfile_index\tpath\tdiscarded\tsource_width\tsource_height\twidth\theight\tjpeg_bytes\trgba_bytes\tfile_bytes_read\tread_calls\twarnings\topen_ms\tdiscover_ms\tread_ms\tdecode_ms\ttotal_ms\trequest_ms")
        .map_err(|e| e.to_string())?;
    for strategy in args.strategies.iter().copied() {
        if args.mode != "contention" {
            let mut samples = Vec::new();
            let mut decoder = Decoder::with_scale(Backend::Turbo, strategy.scale())?;
            let mut jpeg = Vec::new();
            for round in 1..=args.rounds {
                let order = if args.order == "random" {
                    shuffled_order(files.len(), args.seed.wrapping_add(round as u64))
                } else {
                    (0..files.len()).collect()
                };
                let mut batch = Vec::with_capacity(files.len());
                for (request, index) in order.into_iter().enumerate() {
                    let mut sample =
                        measure(&files[index], index, strategy, &mut decoder, &mut jpeg)?;
                    sample.request = request as u64 + 1;
                    batch.push(sample);
                }
                write_samples(
                    &mut output,
                    &files,
                    &Label {
                        phase: "micro",
                        strategy,
                        order: &args.order,
                        round,
                        block: 0,
                        condition: "serial",
                        role: "thumbnail",
                    },
                    &batch,
                )
                .map_err(|e| e.to_string())?;
                samples.extend(batch);
            }
            summarize(
                &format!(
                    "Micro {} (native scale 1/{})",
                    strategy.name(),
                    strategy.scale()
                ),
                &samples,
            );
        }
        if args.mode != "micro" {
            let mut baseline = Vec::new();
            let mut concurrent = Vec::new();
            for round in 1..=args.rounds {
                let order = if args.order == "random" {
                    shuffled_order(files.len(), args.seed.wrapping_add(round as u64))
                } else {
                    (0..files.len()).collect()
                };
                let conditions = if round % 2 == 1 {
                    [false, true, true, false]
                } else {
                    [true, false, false, true]
                };
                for (block, enabled) in conditions.into_iter().enumerate() {
                    let pass = run_pass(&files, &order, strategy, enabled)?;
                    let condition = if enabled {
                        "B-concurrent"
                    } else {
                        "A-baseline"
                    };
                    let mut label = Label {
                        phase: "contention",
                        strategy,
                        order: &args.order,
                        round,
                        block: block + 1,
                        condition,
                        role: "main",
                    };
                    write_samples(&mut output, &files, &label, &pass.main)
                        .map_err(|e| e.to_string())?;
                    label.role = "thumbnail";
                    write_samples(&mut output, &files, &label, &pass.thumbs.samples)
                        .map_err(|e| e.to_string())?;
                    eprintln!("Pass strategy={} round={} block={} {} foreground_count={} foreground_ms={:.3} elapsed_ms={:.3} thumb_completed={} thumb_cache_hits={} thumb_windows={} thumb_ready_windows={} thumb_discarded={} thumb_file_bytes={} thumb_peak_cache={} thumb_peak_RGBA_with_inflight={} thumb_peak_JPEG_capacity={} thumb_budget_skips={}",
                        strategy.name(), round, block + 1, condition, pass.main.len(), pass.foreground_ms, pass.elapsed_ms,
                        pass.thumbs.samples.len(), pass.thumbs.cache_hits, pass.thumbs.windows, pass.thumbs.ready_windows,
                        pass.thumbs.samples.iter().filter(|s| s.discarded).count(),
                        pass.thumbs.samples.iter().map(|s| s.file_bytes_read).sum::<u64>(),
                        pass.thumbs.peak_cache_bytes, pass.thumbs.peak_rgba_bytes, pass.thumbs.peak_jpeg_capacity, pass.thumbs.budget_skips);
                    metric(
                        "main request",
                        &pass.main.iter().map(|s| s.request_ms).collect::<Vec<_>>(),
                    );
                    if enabled {
                        concurrent.extend(pass.main);
                    } else {
                        baseline.extend(pass.main);
                    }
                    output.flush().map_err(|e| e.to_string())?;
                }
            }
            summarize(
                &format!("Contention {} A baseline", strategy.name()),
                &baseline,
            );
            summarize(
                &format!("Contention {} B concurrent thumbnails", strategy.name()),
                &concurrent,
            );
            let a: Vec<_> = baseline.iter().map(|s| s.request_ms).collect();
            let b: Vec<_> = concurrent.iter().map(|s| s.request_ms).collect();
            let mean_a = a.iter().sum::<f64>() / a.len() as f64;
            let mean_b = b.iter().sum::<f64>() / b.len() as f64;
            eprintln!("Observed B/A main-request change: mean={:+.2}% p95={:+.2}% p99={:+.2}% (descriptive only; inspect per-pass noise)",
                (mean_b / mean_a - 1.0) * 100.0,
                (percentile(&b, 95) / percentile(&a, 95) - 1.0) * 100.0,
                (percentile(&b, 99) / percentile(&a, 99) - 1.0) * 100.0);
        }
    }
    output.flush().map_err(|e| e.to_string())
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
                eprintln!("fastcull-filmstrip-bench: {error}");
                ExitCode::FAILURE
            }
        },
        Err(error) => {
            eprintln!("fastcull-filmstrip-bench: {error}\nUse --help for usage.");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn preview(width: u32, height: u32, length: u64) -> EmbeddedPreview {
        EmbeddedPreview {
            offset: 0,
            length,
            width: Some(width),
            height: Some(height),
        }
    }
    #[test]
    fn selection_uses_pixels_and_adequate_size_not_candidate_position() {
        let previews = [
            preview(7008, 4672, 1000),
            preview(160, 120, 5000),
            preview(1616, 1080, 4000),
            preview(800, 600, 3000),
        ];
        assert_eq!(
            select_preview(&previews, Strategy::Tiny).unwrap().width,
            Some(160)
        );
        assert_eq!(
            select_preview(&previews, Strategy::Medium).unwrap().width,
            Some(1616)
        );
        assert_eq!(
            select_preview(&previews, Strategy::MediumEighth)
                .unwrap()
                .width,
            Some(1616)
        );
        assert_eq!(
            select_preview(&previews, Strategy::Large).unwrap().width,
            Some(7008)
        );
        assert_eq!(
            select_preview(&previews[1..2], Strategy::Medium)
                .unwrap()
                .width,
            Some(160)
        );
        assert!(select_preview(&[], Strategy::Tiny).is_err());
    }
    #[test]
    fn window_is_bounded_nearest_first_and_handles_edges() {
        assert_eq!(visible_window(0, 3), vec![0, 1, 2]);
        assert_eq!(visible_window(2, 3), vec![2, 1, 0]);
        assert_eq!(
            visible_window(5, 11),
            vec![5, 6, 4, 7, 3, 8, 2, 9, 1, 10, 0]
        );
    }
    #[test]
    fn seeded_orders_repeat_and_preserve_every_index() {
        let a = shuffled_order(100, 42);
        assert_eq!(a, shuffled_order(100, 42));
        assert_ne!(a, shuffled_order(100, 43));
        let mut sorted = a;
        sorted.sort_unstable();
        assert_eq!(sorted, (0..100).collect::<Vec<_>>());
    }
    #[test]
    fn spread_selection_includes_both_ends_without_duplicates() {
        assert_eq!(spread_indices(433, 5), vec![0, 108, 216, 324, 432]);
        assert_eq!(spread_indices(3, 10), vec![0, 1, 2]);
        assert_eq!(spread_indices(433, 1), vec![0]);
        assert!(spread_indices(0, 64).is_empty());
        let spread = spread_indices(433, 64);
        assert_eq!(spread.len(), 64);
        assert!(spread.windows(2).all(|pair| pair[0] < pair[1]));
        assert_eq!(spread.first(), Some(&0));
        assert_eq!(spread.last(), Some(&432));
    }
    #[test]
    fn nearest_rank_percentiles_are_defined_for_small_samples() {
        assert_eq!(percentile(&[], 95), 0.0);
        assert_eq!(percentile(&[4.0, 1.0, 3.0, 2.0], 50), 2.0);
        assert_eq!(percentile(&[4.0, 1.0, 3.0, 2.0], 95), 4.0);
    }
    #[test]
    fn arguments_limit_work_and_accept_multiple_strategies() {
        let args = Args::parse(
            [
                "photos",
                "--mode",
                "both",
                "--strategy",
                "tiny,medium-eighth",
                "--rounds",
                "2",
            ]
            .map(OsString::from),
        )
        .unwrap()
        .unwrap();
        assert_eq!(args.strategies, [Strategy::Tiny, Strategy::MediumEighth]);
        assert_eq!(args.rounds, 2);
        assert!(Args::parse(
            ["photos", "--mode", "both", "--limit", "10000", "--rounds", "8"].map(OsString::from)
        )
        .is_err());
        assert!(Args::parse(["photos", "--limit", "0"].map(OsString::from)).is_err());
    }
}
