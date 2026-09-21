//! Synthetic, headless same-queue upload contention; never reads photographs.
#![deny(unsafe_code)]

use fastcull::renderer::texture::wait_for_upload_completion;
use std::{
    sync::{
        atomic::{AtomicU8, Ordering},
        mpsc, Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

const HELP: &str =
    "fastcull-upload-contention-bench [--rounds N] [--warmup N] [--width N] [--height N]

Intel Metal synthetic test; no photograph or sidecar I/O.
Default: 24 three-mode rounds, 6 warmup rounds, 7008x4672 RGBA upload.
Each round tests legacy-Wait, write_texture/Poll, and mapped-staging/Poll.
Six rotating/reversed orders balance positions. Nonzero warmup rounds up to six.
--pairs is a compatibility alias for --rounds; it now runs three modes.
While the worker copies/uploads, the foreground submits an empty command list
approximately every millisecond to the SAME queue. TSV samples go to stdout;
summaries go to stderr. A stage sample can straddle staging/completion boundaries.

This isolates CPU queue contention, not display latency, rendering FPS, or GPU
throughput. It deliberately probes more frequently than normal 60 Hz rendering.
Mapped staging uses one MAP_WRITE|COPY_SRC buffer and an explicit texture copy.
queue.write_texture staging copies in the other modes can block foreground submits.
Run with no other benchmarks, builds, captures, or heavy applications active.";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Legacy,
    Poll,
    Mapped,
}

impl Mode {
    fn label(self) -> &'static str {
        match self {
            Self::Legacy => "legacy_wait",
            Self::Poll => "worker_poll",
            Self::Mapped => "mapped_staging_poll",
        }
    }
}

struct Args {
    pairs: usize,
    warmup: usize,
    width: u32,
    height: u32,
}

impl Args {
    fn parse() -> Result<Option<Self>, String> {
        let mut args = Self {
            pairs: 24,
            warmup: 6,
            width: 7008,
            height: 4672,
        };
        let mut values = std::env::args().skip(1);
        while let Some(option) = values.next() {
            if matches!(option.as_str(), "-h" | "--help") {
                return Ok(None);
            }
            let value = values
                .next()
                .ok_or_else(|| format!("missing value for {option}"))?
                .parse::<u32>()
                .map_err(|_| format!("invalid value for {option}"))?;
            match option.as_str() {
                "--pairs" | "--rounds" if (1..=1000).contains(&value) => {
                    args.pairs = value as usize
                }
                "--warmup" if value <= 100 => args.warmup = value as usize,
                "--width" if (1..=16384).contains(&value) => args.width = value,
                "--height" if (1..=16384).contains(&value) => args.height = value,
                _ => return Err(format!("unsupported option or value: {option} {value}")),
            }
        }
        if u64::from(args.width) * u64::from(args.height) * 4 > 256 * 1024 * 1024 {
            return Err("synthetic pixels must fit in 256 MiB".into());
        }
        args.warmup = args.warmup.div_ceil(6) * 6;
        Ok(Some(args))
    }
}

#[derive(Clone, Copy)]
struct Sample {
    phase_start: u8,
    phase_end: u8,
    submit_ms: f64,
}

struct Trial {
    mode: Mode,
    samples: Vec<Sample>,
    upload_submit_ms: f64,
    upload_complete_ms: f64,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("upload contention benchmark: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let Some(args) = Args::parse()? else {
        println!("{HELP}");
        return Ok(());
    };
    let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
        backends: wgpu::Backends::METAL,
        ..Default::default()
    });
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::LowPower,
        compatible_surface: None,
        force_fallback_adapter: false,
    }))
    .ok_or("no Metal adapter")?;
    let (device, queue) = pollster::block_on(adapter.request_device(
        &wgpu::DeviceDescriptor {
            label: Some("FastCull contention experiment"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::downlevel_defaults().using_resolution(adapter.limits()),
            memory_hints: wgpu::MemoryHints::MemoryUsage,
        },
        None,
    ))
    .map_err(|error| error.to_string())?;
    if args.width > device.limits().max_texture_dimension_2d
        || args.height > device.limits().max_texture_dimension_2d
    {
        return Err("requested dimensions exceed adapter limit".into());
    }
    let padded_row = (args.width * 4).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT)
        * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    if u64::from(padded_row) * u64::from(args.height) > device.limits().max_buffer_size {
        return Err("padded staging exceeds device buffer limit".into());
    }
    let errors = Arc::new(Mutex::new(None));
    let callback_errors = Arc::clone(&errors);
    device.on_uncaptured_error(Box::new(move |error| {
        *callback_errors.lock().unwrap_or_else(|e| e.into_inner()) = Some(error.to_string());
    }));
    let device = Arc::new(device);
    let queue = Arc::new(queue);
    eprintln!("Adapter: {:?}", adapter.get_info());
    eprintln!(
        "Pixels: {}x{}, {:.2} MiB; {} three-mode rounds, {} warmup rounds",
        args.width,
        args.height,
        f64::from(args.width) * f64::from(args.height) * 4.0 / (1024.0 * 1024.0),
        args.pairs,
        args.warmup
    );
    let pixels: Arc<Vec<u8>> = Arc::new(
        (0..args.width as usize * args.height as usize * 4)
            .map(|index| (index % 251) as u8)
            .collect(),
    );
    let mut baseline = Vec::with_capacity(100);
    for _ in 0..100 {
        let start = Instant::now();
        queue.submit([]);
        baseline.push(start.elapsed().as_secs_f64() * 1000.0);
        thread::sleep(Duration::from_millis(1));
    }
    summary("foreground submit without upload", baseline);
    let _ = device.poll(wgpu::Maintain::Wait); // No concurrent foreground work yet.
    println!("round\tmode\tphase_start\tphase_end\tforeground_submit_ms\tupload_submit_ms\tupload_complete_ms");
    let mut trials = Vec::new();
    for pair in 0..args.warmup + args.pairs {
        let modes = trial_order(pair);
        for mode in modes {
            let result = trial(&device, &queue, &pixels, &args, mode)?;
            if let Some(error) = errors.lock().unwrap_or_else(|e| e.into_inner()).clone() {
                return Err(error);
            }
            if pair >= args.warmup {
                for sample in &result.samples {
                    println!(
                        "{}\t{}\t{}\t{}\t{:.6}\t{:.6}\t{:.6}",
                        pair - args.warmup,
                        mode.label(),
                        phase_name(sample.phase_start),
                        phase_name(sample.phase_end),
                        sample.submit_ms,
                        result.upload_submit_ms,
                        result.upload_complete_ms,
                    );
                }
                trials.push(result);
            }
        }
    }
    for mode in [Mode::Legacy, Mode::Poll, Mode::Mapped] {
        let matching: Vec<_> = trials.iter().filter(|trial| trial.mode == mode).collect();
        summary(
            &format!("{} foreground submit, all phases", mode.label()),
            matching
                .iter()
                .flat_map(|trial| trial.samples.iter().map(|sample| sample.submit_ms))
                .collect(),
        );
        summary(
            &format!(
                "{} foreground submit starting during completion",
                mode.label()
            ),
            matching
                .iter()
                .flat_map(|trial| &trial.samples)
                .filter(|sample| sample.phase_start == 2)
                .map(|sample| sample.submit_ms)
                .collect(),
        );
        summary(
            &format!("{} worst foreground submit per trial", mode.label()),
            matching
                .iter()
                .filter_map(|trial| {
                    trial
                        .samples
                        .iter()
                        .map(|sample| sample.submit_ms)
                        .max_by(f64::total_cmp)
                })
                .collect(),
        );
        summary(
            &format!("{} upload start to completion", mode.label()),
            matching
                .iter()
                .map(|trial| trial.upload_complete_ms)
                .collect(),
        );
    }
    eprintln!("Headless lock-contention measurements only; not input-to-display latency.");
    Ok(())
}

fn phase_name(phase: u8) -> &'static str {
    match phase {
        0 => "preparing",
        1 => "staging_allocate_copy",
        2 => "completion",
        4 => "queue_submit",
        5 => "mapped_fill_encode",
        _ => "done",
    }
}

fn trial_order(round: usize) -> [Mode; 3] {
    use Mode::{Legacy as A, Mapped as C, Poll as B};
    [
        [A, B, C],
        [B, C, A],
        [C, A, B],
        [C, B, A],
        [B, A, C],
        [A, C, B],
    ][round % 6]
}

fn trial(
    device: &Arc<wgpu::Device>,
    queue: &Arc<wgpu::Queue>,
    pixels: &Arc<Vec<u8>>,
    args: &Args,
    mode: Mode,
) -> Result<Trial, String> {
    let phase = Arc::new(AtomicU8::new(0));
    let worker_phase = Arc::clone(&phase);
    let worker_device = Arc::clone(device);
    let worker_queue = Arc::clone(queue);
    let pixels = Arc::clone(pixels);
    let [width, height] = [args.width, args.height];
    let (ready, begin) = mpsc::sync_channel(1);
    let worker = thread::Builder::new()
        .name("upload-contention".into())
        .spawn(move || -> Result<(f64, f64), String> {
            ready.send(()).map_err(|error| error.to_string())?;
            let start = Instant::now();
            let texture = worker_device.create_texture(&wgpu::TextureDescriptor {
                label: Some("synthetic ARW-sized preview"),
                size: wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8UnormSrgb,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });
            worker_phase.store(1, Ordering::Release);
            let staging = if mode == Mode::Mapped {
                let row = width * 4;
                let padded_row = row.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT)
                    * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
                // MAP_WRITE prevents mapped_at_creation from allocating a
                // second temporary buffer for an otherwise private buffer.
                let staging = worker_device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("explicit shared upload staging"),
                    size: u64::from(padded_row) * u64::from(height),
                    usage: wgpu::BufferUsages::MAP_WRITE | wgpu::BufferUsages::COPY_SRC,
                    mapped_at_creation: true,
                });
                worker_phase.store(5, Ordering::Release);
                {
                    let mut mapped = staging.slice(..).get_mapped_range_mut();
                    for (source, destination) in pixels
                        .chunks_exact(row as usize)
                        .zip(mapped.chunks_exact_mut(padded_row as usize))
                    {
                        destination[..row as usize].copy_from_slice(source);
                    }
                }
                staging.unmap();
                let mut encoder =
                    worker_device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                        label: Some("explicit upload copy"),
                    });
                encoder.copy_buffer_to_texture(
                    wgpu::TexelCopyBufferInfo {
                        buffer: &staging,
                        layout: wgpu::TexelCopyBufferLayout {
                            offset: 0,
                            bytes_per_row: Some(padded_row),
                            rows_per_image: Some(height),
                        },
                    },
                    wgpu::TexelCopyTextureInfo {
                        texture: &texture,
                        mip_level: 0,
                        origin: wgpu::Origin3d::ZERO,
                        aspect: wgpu::TextureAspect::All,
                    },
                    texture.size(),
                );
                let commands = encoder.finish();
                worker_phase.store(4, Ordering::Release);
                worker_queue.submit([commands]);
                Some(staging)
            } else {
                worker_queue.write_texture(
                    wgpu::TexelCopyTextureInfo {
                        texture: &texture,
                        mip_level: 0,
                        origin: wgpu::Origin3d::ZERO,
                        aspect: wgpu::TextureAspect::All,
                    },
                    &pixels,
                    wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(width * 4),
                        rows_per_image: Some(height),
                    },
                    texture.size(),
                );
                worker_phase.store(4, Ordering::Release);
                worker_queue.submit([]);
                None
            };
            let submit_ms = start.elapsed().as_secs_f64() * 1000.0;
            let (sender, completed) = mpsc::sync_channel(1);
            worker_queue.on_submitted_work_done(move || {
                let _ = sender.send(());
            });
            worker_phase.store(2, Ordering::Release);
            let completion = match mode {
                Mode::Legacy => {
                    let _ = worker_device.poll(wgpu::Maintain::Wait);
                    completed.recv()
                }
                Mode::Poll | Mode::Mapped => wait_for_upload_completion(&worker_device, &completed),
            };
            let complete_ms = start.elapsed().as_secs_f64() * 1000.0;
            worker_phase.store(3, Ordering::Release);
            completion.map_err(|error| error.to_string())?;
            drop(staging);
            Ok((submit_ms, complete_ms))
        })
        .map_err(|error| error.to_string())?;
    begin.recv().map_err(|error| error.to_string())?;
    let mut samples = Vec::with_capacity(128);
    loop {
        let phase_start = phase.load(Ordering::Acquire);
        if phase_start == 3 || worker.is_finished() {
            break;
        }
        let start = Instant::now();
        queue.submit([]);
        samples.push(Sample {
            phase_start,
            phase_end: phase.load(Ordering::Acquire),
            submit_ms: start.elapsed().as_secs_f64() * 1000.0,
        });
        thread::sleep(Duration::from_millis(1));
    }
    let (upload_submit_ms, upload_complete_ms) = worker
        .join()
        .map_err(|_| "upload benchmark worker panicked")??;
    // Drain only after the foreground probe loop finishes and before next pair.
    let _ = device.poll(wgpu::Maintain::Wait);
    Ok(Trial {
        mode,
        samples,
        upload_submit_ms,
        upload_complete_ms,
    })
}

fn summary(label: &str, mut values: Vec<f64>) {
    if values.is_empty() {
        eprintln!("{label}: no samples");
        return;
    }
    values.sort_by(f64::total_cmp);
    let percentile = |p: f64| values[(values.len() as f64 * p).ceil() as usize - 1];
    eprintln!(
        "{label}: n={} p50={:.3} p95={:.3} p99={:.3} max={:.3} ms",
        values.len(),
        percentile(0.5),
        percentile(0.95),
        percentile(0.99),
        values[values.len() - 1]
    );
}
