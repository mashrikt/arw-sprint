# Optional filmstrip: Intel Mac measurements

Research date: 2026-09-21. The original experiments used standalone benchmark
tools and left the installed app unchanged. The subsequent production filmstrip
has since been revised for independent scrolling and stable thumbnail residency.
Functional verification of the revision and the prior build are identified
separately below. The timings remain component results, not measurements of
integrated GUI latency.

## Decision and scope

The implementation uses the small embedded **160×120 JPEG** found in the local
A7 V corpus. It was present and successfully decoded in all 433 local files.
The median JPEG was 6,681 bytes; the range was 2,372–13,325 bytes. “7 KB” is a
useful approximation, not a fixed file-format size.

The component measurements support a small, optional strip toggled with Tab.
They do not establish end-to-end UI latency or a zero-overhead guarantee. Keep
the current photograph, its nearest prepared neighbor, and the existing image
budget ahead of thumbnail work. The larger embedded previews remain suitable
for the main viewer; they are considerably more expensive thumbnail sources.

## Desktop implementation

The strip starts hidden. **Tab** toggles it; clicking a cell selects that photo,
while wheel/trackpad scrolling over the strip browses thumbnails independently
and leaves the main photo open. Arrow keys still navigate the main photo.
The strip follows explicit selection only when it leaves a visible edge; it
does not recenter on every arrow press or asynchronous metadata/status update.
It sits above the status
bar, with the main photo and optional pinned comparison fitted above it. Cells
preserve orientation/aspect ratio and show a cached filename/rating caption plus
a selection border. Missing or unsupported thumbnails remain gray placeholders.

The thumbnail decoder runs on one utility-priority thread. A replaceable plan
prioritizes up to 24 visible cells and then a halo of up to 12 neighbors on each
side, with generation checks between stages and
only one decoded result awaiting upload/acknowledgment. It chooses the smallest
suitable embedded JPEG, bounded to 512 pixels per side and 256 KiB compressed.
Native reduced-IDCT decoding limits the output to 192 pixels per side; the
usual Sony 160×120 thumbnail remains unchanged.
It has no medium/full-preview fallback and never develops RAW sensor data.
The typical 160×120 source can appear soft when upscaled on Retina displays.

Uploads use the existing serialized GPU worker; foreground and main-photo
prefetch uploads outrank thumbnails. Thumbnail JPEG/RGBA buffers, textures, and
staging use a fixed **8 MiB pool carved from the configured total**, leaving
504 MiB for main photos at the default 512 MiB setting. It is not added to the
chosen budget or eagerly allocated. A rolling GPU cache retains up to **48
thumbnails / 4 MiB**, protects visible cells, and evicts distant entries first.
Overlapping thumbnails remain cached as the visible window moves. Main-photo
memory pressure cannot clear this pool, preventing repeated blanking and reloads
while navigating. Hiding cancels queued thumbnail work and releases the
strip's retained image handles; submitted frames keep their memory leases until
GPU completion. Completed idle strips do not request periodic redraws.

The renderer skips uniform writes when transforms are unchanged. Selection-only
updates use existing GPU state, and a bounded caption cache reuses labels across
cell positions when scrolling forward or backward. Main-image zoom, pan, and
strip toggling do not decode the main image. This implementation adds no crates.
Functional verification is recorded below; integrated on/off performance
measurements remain pending. No new latency claim is made here.

## Integrated functional verification

### Independent scrolling and rolling cache, 2026-09-21

This evidence covers the filmstrip revision before the subsequent Space/X local
moves and S/D zoom-shortcut changes. That later revision passed 180 tests, strict
Clippy, and formatting checks; its [desktop recheck and build evidence](deleted-workflow.md#verification)
are recorded separately.

All **148 tests**, strict Clippy, and formatting checks passed. The read-only
desktop smoke completed **45 frames** for each of these cases:

| Input | Configured budget | Result |
| --- | ---: | --- |
| Five-file neutral synthetic fixture | 512 MiB | 45/45 pixel checks passed, zero red-biased pixels |
| Actual 433-file A7 V folder | 512 MiB | Passed; 3,773 cached texture identity checks |
| Six-file fixture with 7008×4672 main previews | 512 MiB | Passed; 1,864 cached texture identity checks |
| Same six-file fixture | 256 MiB | Passed; 1,856 cached texture identity checks |

The cached texture checks compare the retained texture identity of overlapping
photos through updates, detecting unnecessary eviction/replacement. The tests
also cover independent strip scrolling, explicit selection, metadata/status
updates, main-photo navigation, comparison, resizing, and hiding/reopening.

In the real-folder run, frames 40 and 41 show different filmstrip contents after
scrolling between the folder's end and start. Their **3,001,920 main-canvas pixels
are byte-identical**, confirming that scrolling did not change the main canvas.
In each six-photo run, all six thumbnails were decoded/uploaded once during the
first strip opening, despite subsequent main-photo changes. The total reached
12 only after the explicit Tab hide/reopen, which intentionally releases and
refills the cache. These are bounded observations of the smoke sequences, not a
latency benchmark or a guarantee for all folders and workloads.

Current evidence is in `bench-results/filmstrip-stability-validation/`, including
`tests.log`, `clippy.log`, `smoke-real512.log`, `smoke-full512.log`, and
`smoke-full256.log`. Pixel reports are
`bench-results/filmstrip-stability-frame-inspection.json` and
`bench-results/filmstrip-stability-real-scroll-comparison.json`.
Capture/readback disturbs timing; this validation does not establish integrated
on/off FPS, input-to-display latency, or idle CPU consumption.

### Prior build, 2026-09-21

**Prior build, verified on 2026-09-21.** These results precede the independent-scroll
and rolling-cache revision above and do not validate its changed behavior.

All **133 tests** and strict Clippy checks passed for that build. The read-only
desktop smoke completed all **40 frames** for both the five-file neutral synthetic
fixture and the actual **433-file A7 V folder at the default 512 MiB budget**. It exercises
Tab toggling, thumbnail clicks, window resizing, rapid forward/reverse navigation,
comparison with the strip visible, then hiding and returning to the full view.

The synthetic pixel checker reused the original frame 0–32 checks and applied
separate filmstrip/comparison geometry checks to frames 33–39. All 40 passed,
with zero red-biased pixels, populated thumbnails, a single complete selection
border, and no thumbnail/border/caption overlap. Visual inspection of real-photo
frames 33 and 37 confirmed distinct sequence-number captions, the selected cell
matching the current photograph, correct aspect/letterboxing, and clean margins
around both comparison panes. A uniform fixture alone cannot establish subject
identity or photo color fidelity.

After hiding the real strip, worker counters settled at **45 jobs started and
45 completed**, with **43 uploads**, and the smoke assertions verified no queued
thumbnail work or retained thumbnail cache. The before/after source manifest
for **449 RAW/XMP files** was unchanged. Local evidence is in
`bench-results/filmstrip-smoke-fixture/`, `bench-results/filmstrip-smoke-real512/`,
and `bench-results/filmstrip-frame-inspection.json`; the read-only checker is
`bench-results/check-filmstrip-frames.py`.

These are functional checks, not a controlled on/off benchmark. Frame capture
and GPU readback disturb timings. Integrated input-to-display latency, strip
fill time, idle CPU, and cache-hit effects remain unmeasured.

## Test machine and protocol

- Intel Core i7-1068NG7, four physical cores/eight threads, 32 GiB RAM.
- Intel Iris Plus integrated graphics; macOS 26.2; internal APFS SSD.
- Existing pinned TurboJPEG/libjpeg-turbo 3.1.0 and wgpu 24.0.5 Metal backend.
- 433 local A7 V ARWs, read-only. No RAW sensor decoding or sidecar writes.
- Interactive machine with other applications active. OS caches, clock speed,
  and thermal state were uncontrolled. No cache purge or cold-SSD claim.
- Builds completed before timings. CPU and GPU experiments ran separately.
  Functional frame capture was not part of any timed pass.

## Choosing the thumbnail source

Each strategy opened every file, discovered supported JPEG candidates through
the existing bounded TIFF parser, read only its selected JPEG, and decoded to
RGBA. One decoder and JPEG buffer were reused per strategy. This single pass
used fixed strategy order, so later strategies could benefit from warmer OS
caches. Values include open/discovery/read/decode, but exclude GPU upload.

| Source and decoded output | Median total | p95 total | p99 total | RGBA per image |
| --- | ---: | ---: | ---: | ---: |
| Small embedded JPEG, 160×120 | 0.369 ms | 1.174 ms | 2.271 ms | 75 KiB |
| Medium JPEG at 1/8, 202×135 | 6.098 ms | 8.371 ms | 9.705 ms | 106.5 KiB |
| Medium JPEG at 1/4, 404×270 | 8.162 ms | 11.031 ms | 12.036 ms | 426.1 KiB |
| Largest JPEG at 1/8, 576×384 or 876×584 | 103.408 ms | 117.463 ms | 122.718 ms | up to 1.95 MiB |

All 1,732 extraction/decode attempts succeeded, with no parser warnings. The
small-thumbnail decode stage alone had a 0.198 ms median and 0.299 ms p95.

The small JPEG does not mean just 7 KB of total reads. Container discovery
also reads metadata and candidate headers: small-thumbnail extraction requested
34,391 bytes per file at the median, 15,090,025 bytes over all 433 files.
These are application-requested bytes, not physical device I/O counters.

One sample's small thumbnail and medium preview were visually compared. The
small image retained the same composition with black bars above and below.
It is adequate for coarse recognition, with visibly less fine detail. Preserve
its aspect ratio and apply the ARW's orientation; do not assume identical
letterboxing in every camera file. Upscaling on Retina displays will look soft.
The main photograph's preview resolution remains unchanged.

## Cached rendering

Three independent runs used 240 paired baseline/filmstrip frames each, with
12 warmup pairs and alternating AB/BA order. Both cases rendered the same
GPU-generated, detailed 7008×4672 texture at the same placement into a
2880×1800 offscreen target. The filmstrip case added twelve cached 160×120
textures, each drawn at 232×174 physical pixels. Keeping the main image the
same avoids crediting the strip for reducing the main image's rendering work.

| Run | Baseline median / p95 | Filmstrip median / p95 | Median paired increment |
| --- | ---: | ---: | ---: |
| 1 | 6.460 / 6.841 ms | 6.601 / 6.949 ms | 0.142 ms |
| 2 | 6.424 / 6.805 ms | 6.556 / 6.938 ms | 0.153 ms |
| 3 | 6.433 / 6.818 ms | 6.559 / 6.865 ms | 0.145 ms |

These are CPU wall times from command encoding through GPU completion, not GPU
timestamp queries, UI-event latency, or display scanout. The pinned Metal
backend's completion wait polls with one-millisecond sleeps; paired p95
increments around 1.4 ms and negative tails around −1 ms include substantial
polling/scheduler noise. The consistent median increment is small, but should
not be interpreted as a precise GPU-only measurement.

Uploading all twelve small textures once took 3.283–4.913 ms, including queue
submission/completion. Their combined GPU texel allocation was **921,600 bytes
(0.879 MiB)**. Retaining a CPU copy of all twelve would double pixel storage to
1.758 MiB; compressed buffers, row-padded staging, metadata, labels, and driver
bookkeeping are additional. Normal rendering reused textures without uploads.
The production renderer also draws selection/placeholder rectangles and cached
labels, which this headless rendering experiment did not include.

## Main-photo interference

The CPU experiment used 64 files evenly spread across the 433-file directory,
including its first and last files. Both 4608×3072 and 7008×4672 main previews
were represented. One run used natural order of that selected subset; another
used seeded random order. Each ran an ABBA round followed by BAAB, with identical
file order inside each round: A decoded the largest JPEG at full resolution;
B did the same while one ordinary-priority thread filled the current ±5
thumbnail window. There was no backlog beyond the latest requested window.

The main decoder reused its buffer but retained no decoded-photo cache. These
are **forced full-preview decode timings**, not normal cached arrow navigation.
The separate thumbnail worker was permitted to run concurrently with the main
decode; this experiment did not use production priority gating. Across both orders,
1,024 main-image decodes were measured, 512 per condition.

| Order | Baseline mean / p95 | With thumbnails mean / p95 | Mean change | p95 change |
| --- | ---: | ---: | ---: | ---: |
| Natural order of sampled files | 179.290 / 222.286 ms | 177.020 / 216.373 ms | −1.27% | −2.66% |
| Seeded random order | 178.503 / 216.942 ms | 178.041 / 217.001 ms | −0.26% | +0.03% |

There was **no consistent main-decode slowdown detected**. Negative changes are
not evidence that thumbnail work accelerates decoding: the host was busy, and
individual matched-file results had considerable scheduling/thermal variation.
Within each round, averaging the two A and two B observations of each file
produced median changes of −1.18% in natural order and −0.26% in random order.
Some individual pairs were slower. This is descriptive research, not a bound
on worst-case interference or a statistically isolated causal result.

The worker fully populated **all 512 requested thumbnail windows before the
next main request**, without discarded jobs or budget skips. It decoded 256
thumbnails across the natural-order concurrent passes and 2,314 across random
passes; retained windows supplied the other cache hits. Peak retained CPU
thumbnail pixels were 844,800 bytes (eleven 160×120 images), with a separate
compressed-buffer peak of 13,325 bytes. These readings show useful work, but do
not establish strip-fill latency during 60 Hz cached navigation.

The experiment did not run the production main-image prefetch worker, shared
512 MiB eviction policy, Metal upload queue, GUI input loop, thumbnail labels,
or selection UI. Those remain the scope of the integrated on/off trial.

## Implemented constraints and remaining validation

1. Visible cells use bounded tiny JPEGs, with no large-preview fallback.
2. Current-photo work and main-photo uploads take priority. Generation checks
   discard work that no longer serves the current visible/prefetch window.
   An in-flight native decoder call or GPU upload cannot be interrupted, which
   makes the per-thumbnail bounds important.
3. An 8 MiB thumbnail pool is carved from the configured total; retained GPU
   thumbnails have a 48-entry / 4 MiB cap. Visible entries are protected from
   main-photo memory pressure. CPU ownership is released after upload/cancellation,
   and GPU ownership after completed use.
4. Tab hides/shows the strip without re-decoding the main photo. Hidden strips
   admit no thumbnail work; a filled idle strip causes no periodic redraws.
5. Orientation comes from bounded TIFF discovery. Aspect ratio is preserved,
   and selection/rating indicators are cached.
6. Strip scrolling is independent of the main selection. Explicit selection
   reveals a cell only when necessary; metadata/status updates cannot recenter it.

An integrated on/off performance trial still needs to cover held arrows,
direction changes, large jumps, filter changes, and 256/512 MiB image budgets.
Measure input-to-present latency, main-photo cache hit rate, upload queue delay,
peak managed memory, and idle CPU. A component benchmark cannot prove that the
new cache policy preserves prepared neighbors or that a real strip fills quickly.

## Reproduction and evidence

The two standalone tools use existing dependencies only:

```sh
cargo build --release --locked --features desktop \
  --bin fastcull-filmstrip-bench --bin fastcull-filmstrip-gpu-bench

target/x86_64-apple-darwin/release/fastcull-filmstrip-bench "$PHOTO_DIR" \
  --mode micro --strategy all --limit 433 --rounds 1

target/x86_64-apple-darwin/release/fastcull-filmstrip-bench "$PHOTO_DIR" \
  --mode contention --strategy tiny --limit 64 --spread --rounds 2 --order sequential

target/x86_64-apple-darwin/release/fastcull-filmstrip-bench "$PHOTO_DIR" \
  --mode contention --strategy tiny --limit 64 --spread --rounds 2 --order random

target/x86_64-apple-darwin/release/fastcull-filmstrip-gpu-bench \
  --pairs 240 --width 2880 --height 1800
```

CPU TSV samples go to stdout; summaries go to stderr. GPU output contains both
summary text and paired TSV rows. Local raw results, environment description,
extracted quality samples, and summary data live in
`bench-results/filmstrip-research/` (ignored by version control, with local paths
and private photo derivatives). The installed app's SHA-256 was verified
unchanged at the end of the original research, before the subsequent GUI work.
Six focused CPU-benchmark unit tests and strict Clippy checks passed for that
research; the GPU harness ran successfully on the actual Intel adapter.

See [official source facts and measurement limits](filmstrip-sources.md) for
decoder scaling, macOS QoS, orientation, and cache-budget considerations.
