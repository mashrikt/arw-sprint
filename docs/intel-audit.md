# Intel audit · 21 September 2026

This audit checks FastCull's actual Intel build and measures specific choices on
the development Mac. It does not establish performance on every Intel Mac or
every Sony RAW format. The [README](../README.md) gives the short overview;
[earlier measurements](measurements.md) retain extraction and workflow history.

## Machine and inputs

- MacBook Pro: Intel Core i7-1068NG7, 2.30 GHz, four physical/eight logical cores,
  32 GiB RAM, Intel Iris Plus Graphics, macOS 26.2.
- Target and packaged executable: `x86_64-apple-darwin`, Metal backend only.
  The configured deployment floor is macOS 10.15; that older OS was not tested.
- Real workflow corpus: 433 local Sony A7 V `.ARW` photographs. GUI smoke runs
  read the originals without saving ratings, moving photos, or writing sessions.
- Full-size decode fixture: six copies of real ARWs with 7008×4672 JPEG previews.
  Repeated fixture decodes are not 108 independent photographs or cold-disk tests.
- GPU contention fixture: synthetic 7008×4672 RGBA pixels, 124.90 MiB per image.
  This isolates uploads and queue contention from disk and JPEG decoding.

## What was checked and fixed

**The JPEG build uses Intel SIMD.** The pinned TurboJPEG 1.5.1 wrapper builds
libjpeg-turbo 3.1.0 with CMake Release, `-O3 -DNDEBUG`, NASM, `WITH_SIMD=ON`, and
`REQUIRE_SIMD=ON`. The native library contains SSE2 and AVX2 implementations and
selects supported routines at runtime. The project does not force AVX2 or use
`target-cpu=native`. Requiring SIMD at build time requires assembler support; it
does not eliminate scalar paths. The Rust target has its own baseline features,
so this is not a claim that the entire app requires only SSE2.
[Upstream CPU dispatch](https://github.com/libjpeg-turbo/libjpeg-turbo/blob/3.1.0/simd/x86_64/jsimd.c),
[Rust target CPU option](https://doc.rust-lang.org/rustc/codegen-options/index.html#target-cpu).

**Foreground navigation gets a dedicated decoder.** One foreground worker and
one speculative worker keep work bounded on this four-core laptop. Latest-target
requests replace pending work; checks between stages reject stale jobs. A native
decode already running must finish. Increasing worker count was not assumed to
help: it could compete for CPU and memory bandwidth. Tiny filmstrip work uses
utility priority and an independent bounded cache.
[Apple task priority guidance](https://developer.apple.com/library/archive/documentation/Performance/Conceptual/power_efficiency_guidelines_osx/PrioritizeWorkAtTheTaskLevel.html).

**Idle memory waits now sleep until something changes.** A budget-constrained
decoder previously woke once a second even without useful work. A condition
variable now waits for a memory release, changed request, or shutdown. Regression
tests cover notifications arriving before the wait, spurious wakeups, and
cancellation while the budget is held. This removes an avoidable idle timer;
it is not a measured whole-machine power-saving percentage.
[Apple energy guidance](https://developer.apple.com/library/archive/documentation/Performance/Conceptual/power_efficiency_guidelines_osx/Timers.html).

**GPU completion no longer holds up foreground submission.** In pinned wgpu 24,
a blocking device poll holds a fence lock also needed by queue submission.
Moving that blocking call to a background thread alone did not remove contention.
The upload worker now polls nonblockingly and waits outside that lock, only while
an upload is active. Its completion callback controls when staging reservations
can be released. The idle event loop still sleeps.
[Pinned device code](https://github.com/gfx-rs/wgpu/blob/v24.0.5/wgpu-core/src/device/resource.rs),
[pinned queue code](https://github.com/gfx-rs/wgpu/blob/v24.0.5/wgpu-core/src/device/queue.rs).

**Large uploads use explicit staging.** The convenience texture-write path also
held an internal queue lock while allocating and copying a full RGBA image.
A mapped, CPU-writable staging buffer moves that preparation outside the queue
lock, followed by a short submission to copy into the GPU texture. Row padding,
the staging allocation, and the destination texture remain budgeted. Staging
stays alive until GPU completion. This follows the Intel/AMD resource model of
CPU-accessible upload memory and GPU-private textures; it does not assume Apple
Silicon shared texture behavior.
[Apple Intel-era resource guidance](https://developer.apple.com/library/archive/documentation/3DDrawing/Conceptual/MTLBestPracticesGuide/ResourceOptions.html).

**Decoded sizes are checked before allocation.** The main decoder now validates
the JPEG header against dimensions used to reserve memory before native decode,
as the thumbnail decoder already did. If a source changes between discovery and
reading, mismatched dimensions fail instead of temporarily escaping accounting.
This checks bounded headers, not the RAW sensor payload.

**Zoom and focus now carry across photos by default.** New installations and
legacy saved preferences enable retention; `L` remains an explicit opt-out that
is saved thereafter. A new photo's size and orientation are applied together,
preventing an intermediate unrotated shape from clamping away the focal point.
Texture release during loading retains geometry. Fit/fill retain their modes;
custom/100% views retain scale and normalized focus, subject to the next image's
edges. These are transform changes and do not request another decode.

## JPEG acceleration comparison

Each process decoded the same six full-size previews three times, sequentially:
18 successful decodes. The sequence was automatic SIMD, forced SSE2, SIMD
disabled, then those modes in reverse order. All 108 decodes succeeded.
The diagnostic `JSIMD_*` variables were cleared between modes. These overrides
are benchmark controls, not application settings.

| Mode | First run p50 / p95 | Reversed-order run p50 / p95 |
| --- | ---: | ---: |
| Automatic Intel SIMD | 179.905 / 372.174 ms | 178.825 / 192.001 ms |
| Forced SSE2 | 210.887 / 217.384 ms | 211.239 / 281.685 ms |
| SIMD disabled | 377.822 / 558.619 ms | 381.591 / 432.177 ms |

Automatic SIMD was about **2.1× faster at the median** than disabling it.
SSE2 was also substantially faster than scalar on this machine; that is not a
substitute for testing an older physical CPU. Small sample counts, warm repeated
files, OS caching, thermal state, and other processes limit tail conclusions.
These are JPEG decode times, not cached navigation or input-to-display latency.
The serial CLI reuses output buffers; desktop buffers transfer ownership to the
cache, so these measurements are not interchangeable with the desktop pipeline.

## GPU contention comparison

The included `fastcull-upload-contention-bench` repeatedly submits an empty
foreground command while a background thread uploads one full-size texture.
All six permutations of the three modes rotate through six warmup rounds and
24 measured rounds. Each mode has 24 measured uploads; every trial includes
texture creation. Percentiles use nearest rank. The no-upload foreground
baseline had p50 **0.042 ms**, p95 **0.110 ms**, and maximum **0.735 ms**.

| Upload method | Worst foreground submit per trial, p50 / p95 | Maximum submit | Upload completion p50 |
| --- | ---: | ---: | ---: |
| Texture write + blocking wait | 20.854 / 78.267 ms | 89.001 ms | 33.051 ms |
| Texture write + nonblocking poll | 21.216 / 25.491 ms | 30.275 ms | 32.692 ms |
| Mapped staging + nonblocking poll | 0.194 / 1.270 ms | 2.479 ms | 39.630 ms |

Nonblocking completion alone removed fence-wait stalls but left the large
staging-copy stall. Explicit mapped staging improved foreground submission
substantially at a roughly **7 ms median upload cost** versus texture write with
nonblocking polling. That tradeoff favors responsive navigation on this Intel
Mac. The mapped path's worst foreground submit stayed below 3 ms in this run.
This is a headless lock-contention experiment, **not a measured guarantee of
sub-16 ms display latency**, and an empty foreground command understates real
rendering work. Discrete AMD graphics have not been benchmarked.

The same experiment at the corpus's **160×120 thumbnail size** checked the
shared upload path: texture write with nonblocking polling completed in
1.434 ms median versus 1.412 ms with mapped staging. The mapped path's worst
foreground submit per trial was 0.063 ms median / 0.126 ms p95 (0.176 ms maximum).
This small run did not show a median upload penalty for thumbnails; it does not
isolate every component of filmstrip overhead.

## Reproduce

```sh
cargo build --release --locked --features turbo --bin fastcull-bench \
  --target x86_64-apple-darwin
./target/x86_64-apple-darwin/release/fastcull-bench /path/to/arws \
  --limit 6 --passes 3 --order sequential --decoder turbo

cargo run --release --locked --features desktop \
  --target x86_64-apple-darwin --bin fastcull-upload-contention-bench -- \
  --rounds 24 --warmup 6
```

For the SIMD experiment, use a fresh benchmark process for each of automatic
dispatch, `JSIMD_FORCESSE2=1`, and `JSIMD_FORCENONE=1`, clearing other `JSIMD_*`
overrides first. Do not leave diagnostic overrides set when using the app.
Raw local logs are under ignored `bench-results/intel-audit/`.

## Final build verification

The revised source passed **195 tests**, formatting checks, and Clippy with
`--all-features --all-targets -D warnings` on `x86_64-apple-darwin`. Tests include
session migration and explicit L-off persistence, custom/100% zoom and focus,
orientation changes, loading gaps, staging row padding and invalid sizes,
completion ownership, memory wakeups/cancellation, and JPEG dimension changes.
The release app was rebuilt and its Intel-only architecture and local signature
verified. Packaging preserved the previous bundle without restarting it.

Four read-only desktop smoke runs each completed **45 captured frames**:

| Input | Managed budget | Navigation requests | GPU cache hits | Result |
| --- | ---: | ---: | ---: | --- |
| Folder containing 433 Sony A7 V ARWs | 512 MiB | 43 | 13 | Pass |
| Six full-size preview ARWs | 512 MiB | 30 | 8 | Pass |
| Same six ARWs, reduced decoding | 256 MiB | 30 | 12 | Pass |
| Five synthetic neutral-image ARWs | 512 MiB | 31 | 15 | Pass |

The tests exercised navigation, rapid skips, filters, zoom/pan, resize,
fullscreen, pinned comparison, and independent filmstrip scrolling. Assertions
checked custom and 100% scale/focus retention during navigation and S/D shortcuts
without texture replacement or new main-photo requests. Those specifically
instrumented zoom transitions used GPU cache hits; loading-gap behavior is also
covered by unit tests. The real-photo run checked 3,719 retained visible-thumbnail
identities without an unexpected replacement. Its filmstrip-scroll frame pair
had different strip pixels but identical main-canvas pixels. The 45 neutral
frames passed background, image-presence, filmstrip, and palette checks with
**zero red-biased pixels**. A uniform fixture does not establish real-photo color
fidelity. All 433 original RAWs and two sidecars retained their paths, inodes,
sizes, and modification timestamps.

The folder test navigates a subset of the 433 photographs; it is not a complete
decode of every file in that run. Capture readback disturbs timings, so these
functional runs do not supply a new input-to-display benchmark or idle CPU/RSS
measurement. Photos, captures, and local logs are excluded from the commit.

## Scope and limits

The image budget limits managed image allocations, not total process RSS. Driver
caches, native scratch space, presentation surfaces, and allocator overhead also
consume memory. A full-size RGBA image is about 125 MiB, so the 512 MiB default
cannot keep eleven such neighbors decoded and uploaded simultaneously. The
256 MiB setting uses reduced-resolution decoding to make transitions fit.

The audit does not establish a 2,000/10,000-photo soak result, compatibility with
every A7 V RAW mode or Sony camera, older macOS runtime coverage, or an optimal
worker count for every Intel model. It supports keeping metadata-directed JPEG
reads, runtime SIMD, bounded directional caches, reusable Metal textures, tiny
cached thumbnails, and event-driven scheduling. Uncached full-size photos on
this machine exceed the aspirational 100 ms target; cached navigation is a
different path and avoids that decode.
