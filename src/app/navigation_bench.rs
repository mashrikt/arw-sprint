//! Read-only desktop burst measurement. Uses the real loader, uploads and renderer,
//! without frame readback. Scheduled requests are not physical keyboard events.
use super::{ActiveEventLoop, App, Duration, Instant};

const STEPS: usize = 32;
const WARMUP: Duration = Duration::from_secs(3);

pub(super) struct NavigationBench {
    started: Instant,
    pub next: Instant,
    interval: Duration,
    warm: bool,
    pub actual_size: bool,
    steps: usize,
    initial_hits: [u64; 3],
    shown: Vec<f64>,
    dispatch_late: Vec<f64>,
    immediate: Vec<bool>,
    last_requested: Instant,
    peak_bytes: usize,
}

impl NavigationBench {
    pub fn new(milliseconds: u64, actual_size: bool) -> Self {
        let now = Instant::now();
        Self {
            started: now,
            next: now,
            interval: Duration::from_millis(milliseconds),
            warm: false,
            actual_size,
            steps: 0,
            initial_hits: [0; 3],
            shown: Vec::with_capacity(STEPS),
            dispatch_late: Vec::with_capacity(STEPS),
            immediate: Vec::with_capacity(STEPS),
            last_requested: now,
            peak_bytes: 0,
        }
    }

    pub fn presented(&mut self, elapsed: Duration) {
        if self.steps > 0 {
            self.shown.push(elapsed.as_secs_f64() * 1000.0);
        }
    }
}

fn percentile(samples: &[f64], percent: usize) -> f64 {
    let mut sorted = samples.to_vec();
    sorted.sort_by(f64::total_cmp);
    if sorted.is_empty() {
        return 0.0;
    }
    sorted[(sorted.len() * percent).div_ceil(100).saturating_sub(1)]
}

pub(super) fn tick(app: &mut App, event_loop: &ActiveEventLoop) {
    let Some(mut bench) = app.navigation_bench.take() else {
        return;
    };
    let now = Instant::now();
    bench.peak_bytes = bench.peak_bytes.max(app.managed_used());
    if bench.started.elapsed() > Duration::from_secs(120) {
        app.fatal = Some("navigation benchmark timed out".into());
        event_loop.exit();
    } else if !bench.warm {
        if !app.scanning
            && app.current().is_some()
            && app.current() == app.displayed
            && !app.first_render
        {
            if app
                .files
                .len()
                .saturating_sub(app.navigator.index().unwrap_or(0))
                <= STEPS
            {
                app.fatal =
                    Some("navigation benchmark requires at least 33 photos from its start".into());
                event_loop.exit();
            } else {
                bench.warm = true;
                bench.next = now + WARMUP;
                if bench.actual_size {
                    if let Some(renderer) = &mut app.renderer {
                        renderer.viewport_mut().actual_size();
                    }
                    app.redraw();
                }
            }
        } else {
            bench.next = now + Duration::from_millis(20);
        }
    } else if bench.steps < STEPS && now >= bench.next {
        if bench.steps == 0 {
            bench.initial_hits = [app.gpu_hits, app.cpu_hits, app.prefetch_hits];
            println!("NAV BENCH: cache={}MiB interval={}ms view={} steps={STEPS} warmup=3s; request-to-present submission, not screen scanout",
                app.managed_limit() / 1048576, bench.interval.as_millis(),
                if bench.actual_size { "100%" } else { "fit" });
            if let Some(window) = &app.window {
                println!(
                    "window_physical={:?} display_scale={} filmstrip={}",
                    window.inner_size(),
                    window.scale_factor(),
                    app.filmstrip_visible
                );
            }
            println!("step\tgpu_ready_at_request\tdispatch_late_ms");
        }
        let late = now.saturating_duration_since(bench.next).as_secs_f64() * 1000.0;
        bench.dispatch_late.push(late);
        let old_hits = app.gpu_hits;
        app.navigate(1);
        let immediate = app.gpu_hits > old_hits;
        bench.immediate.push(immediate);
        bench.steps += 1;
        bench.last_requested = Instant::now();
        println!("{}\t{}\t{late:.3}", bench.steps, immediate);
        // Do not replay missed timer ticks in a catch-up burst. Report lateness
        // explicitly and give each requested frame one intended interval.
        bench.next = now + bench.interval;
    } else if bench.steps == STEPS {
        if app.current() == app.displayed && !app.first_render {
            println!("NAV BENCH PASS: requested={} presented={} superseded={} gpu_hits={} cpu_hits={} prefetch_hits={} peak_sampled_managed_mib={:.1}",
                STEPS, bench.shown.len(), STEPS.saturating_sub(bench.shown.len()),
                app.gpu_hits - bench.initial_hits[0], app.cpu_hits - bench.initial_hits[1],
                app.prefetch_hits - bench.initial_hits[2], bench.peak_bytes as f64 / 1048576.0);
            println!("presented_only_latency_ms p50={:.3} p95={:.3}; dispatch_late_ms p50={:.3} p95={:.3}; final_request_to_present_ms={:.3}",
                percentile(&bench.shown, 50), percentile(&bench.shown, 95),
                percentile(&bench.dispatch_late, 50), percentile(&bench.dispatch_late, 95),
                bench.shown.last().copied().unwrap_or_default());
            println!("Cache-ready runs: {:?}; superseded requests are intentionally not counted as fast displays.", bench.immediate);
            event_loop.exit();
        } else if bench.last_requested.elapsed() > Duration::from_secs(15) {
            app.fatal = Some("final navigation target did not become visible within 15s".into());
            event_loop.exit();
        } else {
            bench.next = now + Duration::from_millis(20);
        }
    }
    // Retain diagnostic mode through shutdown so input cannot enable writes.
    app.navigation_bench = Some(bench);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles_include_slow_tail_and_empty_samples_are_safe() {
        assert_eq!(percentile(&[], 95), 0.0);
        assert_eq!(percentile(&[4.0, 1.0, 100.0, 2.0], 50), 2.0);
        assert_eq!(percentile(&[4.0, 1.0, 100.0, 2.0], 95), 100.0);
        let mut bench = NavigationBench::new(80, false);
        bench.presented(Duration::from_millis(300));
        assert!(
            bench.shown.is_empty(),
            "startup must not count as burst latency"
        );
        bench.steps = 1;
        bench.presented(Duration::from_millis(5));
        assert_eq!(bench.shown, [5.0]);
    }
}
