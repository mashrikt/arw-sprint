# Rapid culling investigation, 30 September 2026

This is a personal Intel Mac app. The goal is to keep the existing design simple,
preserve full-detail viewing, and measure small changes before adding machinery.

## Why a fast burst can catch up with the cache

The app already has separate foreground and prefetch decoders, a GPU upload
worker, and background file/metadata workers. TurboJPEG uses Intel SIMD; Metal
handles display transforms. Another async runtime would not make a JPEG decode
faster.

Eight ahead and three behind describe the scheduling window, not guaranteed
resident frames. One 7008×4672 RGBA preview occupies 124.9 MiB. Eight such decoded
previews occupy about 999 MiB before current GPU textures, upload staging and
other buffers. The default managed budget is 512 MiB, including 8 MiB reserved
for thumbnails. The CPU cache soft target is half the main pool.

A larger buffer can absorb a longer burst, but continuous navigation eventually
catches up when requests arrive faster than decoding and uploading finish.
Increasing the look-ahead count alone does not create memory or decode capacity.

## Small changes retained

- Keep a completed, still-wanted speculative decode when navigation changes.
  Do not start stale work, reuse GPU duplicates, or evict nearer images for a
  distant completed result. Existing byte reservations remain authoritative.
- Repair a cancellation handoff: returning to a just-canceled image must restore
  its foreground request once the old owner retires. This is a separate edge
  case from normal cache exhaustion, not the explanation for every loading label.

The normal scheduling window, image quality, cache default, dependencies and
worker count are unchanged. There is no predictive navigation system or
progressive image replacement.

## Tests on this Mac

Intel Core i7-1068NG7, 4 physical / 8 logical cores, 32 GiB RAM, Intel Iris Plus,
macOS 26.2. Other desktop applications and a separate photo-processing job were
active. No other application was stopped. These are loaded-desktop measurements,
not isolated best-case timings or cold-disk benchmarks.

All **433 local ARWs** passed largest-preview discovery, extraction and JPEG
decoding: 430 previews at 7008×4672 and 3 at 4608×3072. The 464 source files,
including sidecars, were checked by path, inode, size and modification time.
The scan was nonrecursive; source files were read only. This validates this
corpus, not every Sony camera or RAW mode. The final Rust checks passed all
254 tests, formatting, and Clippy with warnings denied.

In that full-corpus serial run, median preview reading was 8.5 ms and median JPEG
decoding was 259.3 ms. The existing serial benchmark reuses its output allocation;
these numbers are not GUI latency or a claim about uncached SSD speed.

A separate fresh-allocation test used 12 files, 24 decodes per combination, and
forward/reversed orders. Full-size decode throughput was:

| Concurrent busy decoders | First run, photos/s | Reversed run, photos/s |
| --- | ---: | ---: |
| 1 | 5.33 | 4.67 |
| 2 | 10.44 | 5.86 |
| 3 | 13.59 | 4.34 |

All workers in this probe decode; it reserves no worker for an interactive
foreground request. Disk, GPU and cache scheduling are excluded. Large variation
under background load means these results do not justify changing the app's
worker count. More threads have potential throughput benefits but are not a
free improvement to interactive latency.

Half-width/height decoding uses one quarter of the pixel memory. In an initial
24-file serial comparison its median was 137.6 ms versus 177.7 ms at full size;
the reversed run was noisy and did not confirm that speed advantage. Reduced
output is unsuitable as the sole source for full-detail 100% inspection, so the
normal desktop decode policy was not changed.

## Desktop burst comparison

The same starting image was used for 32 forward requests at 80 ms intervals
(12.5 requests/second), after a three-second warmup. The window was 2560×1504
physical pixels at display scale 2, with the filmstrip hidden. Full-size JPEG
quality and the original 8-ahead/3-behind policy were used throughout.

| Managed budget | View | Ready GPU hits / 32 | Frames presented / 32 | Superseded requests | Sampled peak MiB |
| --- | --- | ---: | ---: | ---: | ---: |
| 512 MiB | Fit | 2 | 3 | 29 | 500.2 |
| 1024 MiB | Fit | 6 | 7 | 25 | 999.8 |
| 2048 MiB | Fit | 10 | 11 | 21 | 1499.4 |
| 512 MiB | 100% | 2 | 3 | 29 | 500.2 |
| 2048 MiB | 100% | 11 | 12 | 20 | 1629.5 |

In these runs the GPU-ready hits formed the beginning of the burst. Reported
median presentation-submission times were about 13–14 ms, but apply **only to
frames actually presented**. The many superseded requests are not hidden cache
hits. Final uncached targets still took about 258–456 ms after the last request.
Timer-dispatch lateness p95 was about 1.1 ms; this is not physical input latency.

A second comparison in reversed cache-size order again produced 2 GPU-ready
hits at 512 MiB and 10 at 2048 MiB. It reached 14.1/15.4 ms presented-frame
medians and 296/416 ms final-target latency, respectively.

A larger existing cache extended the smooth burst without reducing image quality
or introducing a prediction algorithm. It did not make sustained full-resolution
decoding keep up with 12.5 requests/second. This small, loaded-desktop experiment
supports trying the existing larger budget, not a universal speed guarantee.

## Repeat the measurements

The commands below only read photographs. Use the same starting image, window
size and display scale when comparing desktop runs. Avoid other heavy work for
controlled performance comparisons.

```sh
cargo build --release --locked --features desktop --target x86_64-apple-darwin \
  --bin fastcull --bin fastcull-decode-throughput-bench --bin fastcull-bench

# Whole-corpus decode correctness and serial timings.
./target/x86_64-apple-darwin/release/fastcull-bench /path/to/photos \
  --decoder turbo --order sequential

# Existing decoder, different thread counts and output sizes. Repeat reversed.
./target/x86_64-apple-darwin/release/fastcull-decode-throughput-bench /path/to/photos --passes 2
./target/x86_64-apple-darwin/release/fastcull-decode-throughput-bench /path/to/photos --passes 2 --reverse

# Real desktop pipeline, no screenshot readback or preference writes.
# Needs at least 33 photos from the selected starting image. Try 512/1024/2048.
./target/x86_64-apple-darwin/release/fastcull /path/to/start.ARW \
  --cache-mb 512 --navigation-bench-ms 80
# Add --navigation-bench-zoom to test with the viewport at 100%.
```

The desktop probe warms for three seconds after startup, then sends 32 forward
requests. It reports GPU-ready hits, first presentation-submission latency,
superseded requests, timer lateness and sampled managed memory. It does not
measure display scanout or physical keyboard latency. Superseded images are
counted separately, never reported as instant displays. Require process exit
zero **and** `NAV BENCH PASS`; closing the test early is not a passing run.

## Established approaches checked

- [libjpeg-turbo's SIMD and scaling support](https://github.com/libjpeg-turbo/libjpeg-turbo)
  provides the existing decoder acceleration and reduced-resolution IDCT. There
  is no need to build a JPEG decoder or introduce another runtime.
- [TurboJPEG's DCT guidance](https://github.com/libjpeg-turbo/libjpeg-turbo/blob/main/src/turbojpeg.h)
  says fast and accurate IDCT perform similarly on AVX2 x86 CPUs. Switching to
  the setting named fast is not a reliable speed win and can reduce accuracy.
- [Apple's image-memory guidance](https://developer.apple.com/documentation/xcode/making-changes-to-reduce-memory-use)
  recommends sizing decoded images to their display needs. This is a reasonable
  future fit-view optimization, but automatic full-detail refinement would add
  complexity that is outside the current scope.
- [Sequential read-ahead](https://kernel.org/doc/html/v6.12/core-api/mm-api.html)
  is an established bounded prefetch approach. Its Linux page-cache implementation
  is a conceptual reference, not code or an API to add to this macOS app. Reading
  compressed bytes ahead alone cannot remove JPEG decode cost.

Broader physical hardware and camera validation still needs those machines and
files. A larger controller refactor would improve organization, not directly
make the ninth photo appear faster, so it was not mixed into this work.
