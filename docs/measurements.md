# Intel measurements

The [Intel audit](intel-audit.md) records the later SIMD comparison, upload
contention benchmark, and fixes. The measurements below retain their original
scope and revision context.

Measured 2026-09-21 on an Intel Core i7-1068NG7 @ 2.30 GHz, four physical cores,
eight logical CPUs, 32 GiB RAM, macOS 26.2, on AC power. Rust/Cargo 1.86.0;
release binaries are Mach-O x86_64 only. TurboJPEG's native build was verified
as `Release`, `-O3 -DNDEBUG`, `WITH_SIMD=ON`, `REQUIRE_SIMD=ON`, using NASM 3.02.
The pinned wrapper is turbojpeg 1.5.1, bundled native libjpeg-turbo 3.1.0;
the comparison backend is zune-jpeg 0.5.15 / zune-core 0.5.1.

The earlier CLI measurements below cover JPEG-to-RGBA decoding, not RAW
development, GPU upload, app startup, event latency, or cached navigation. The
desktop, asynchronous workers, and managed caches are now implemented. Desktop
smoke evidence is recorded separately; neither data set establishes a general
sub-16 ms presentation guarantee or sub-100 ms uncached latency.

## Desktop baseline smoke observation

The first desktop smoke run opened the 433-file local folder with the 512 MiB
managed image budget and full-resolution decoding. It exercised forward/backward
navigation, rapid skips, zoom/pan and resizing while capturing the application's
own frames. The run completed with `SMOKE PASS`. Source photographs were read-only;
the automated sequence did not change ratings or invoke Trash.

| Observation | Recorded result |
| --- | ---: |
| Directory scan/sort | 1.42 ms |
| Navigation requests | 9 |
| Existing GPU texture hits | 2 |
| Those two request-to-first-frame submissions | 4.01 / 4.31 ms |
| Decoded CPU cache hits | 1 |
| Prefetched results used | 2 |
| Initial request-to-first-frame submission | 243.72 ms |
| Slowest observed request-to-first-frame submission | 519.02 ms |
| Final managed allocation count | 411.8 / 512 MiB |

These are individual observations, not percentiles. Frame submission is not
physical display scanout. Capturing frames adds readback overhead. Managed
allocations are not process RSS: native decoder/driver/presentation and ordinary
application allocations add overhead. Nine requests are not a 433-photo navigation
soak, held-key throughput test, or idle/thermal validation.

This baseline showed fast texture reuse but substantial upload latency for some
CPU-ready images: observed GPU completion intervals were 13.42–210.76 ms. It
motivated preuploading the nearest neighbor on the existing upload worker with
adaptive eviction under the shared budget. **These figures describe the earlier
CPU-prefetch implementation; they are not results for the new GPU-prefetch path.**
The 8-ahead/3-behind interest window does not guarantee that all those images fit.

Raw baseline log: `bench-results/gui-smoke-1.log` (local, ignored).

## GPU-prefetch desktop checks

Subsequent app-bundle executable runs exercised GPU prefetch, ten immediate
navigation requests, direction changes, fit/100%, pan, resize, and entering/leaving
fullscreen. Both completed with `SMOKE PASS`. The 256 MiB run explicitly opened
`DSC00543.ARW`; its orientation-8 portrait was visually checked and appeared upright.
No rating or Trash action was performed on the supplied photographs.

| Observation | 512 MiB, full pixels | 256 MiB, half dimensions |
| --- | ---: | ---: |
| Photos indexed | 433 | 433 |
| Requests / GPU hits | 17 / 2 | 16 / 3 |
| Observed displayed cache-hit submission | 5.83 ms | 15.19 / 10.42 ms |
| Initial first-frame submission | 607.82 ms | 247.83 ms |
| Final managed bytes | 411.8 MiB | 102.9 MiB |
| Maximum RSS (`time -l`) | 810.8 MiB | 353.4 MiB |
| Peak memory footprint (`time -l`) | 657.0 MiB | 319.4 MiB |

Some cache hits were superseded within the ten-request burst before presentation,
so hit counts and presented frames differ. One full-size run encountered a slow
117.15 ms directory enumeration and exercised provisional/final scan publication.
It exposed a selection issue, subsequently fixed with regression tests: without
user navigation the final list now selects the naturally first photograph, or
the explicitly opened file, instead of retaining a provisional first entry.

These are short functional checks with synchronous diagnostic frame captures;
they are not statistically controlled latency or cold-SSD benchmarks. Uncached
full-size files still take hundreds of milliseconds on this Intel Mac. Managed
image reservations stay bounded, but actual RSS exceeds that budget because of
driver, presentation, decoder, and diagnostic readback allocations.
Logs: `bench-results/gui-final-512.log` and `gui-final-256.log` (local, ignored).

The final packaged Finder-style launch was also checked using an isolated
TIFF/JPEG fixture outside Desktop. It exposed and fixed AppKit overwriting the
custom open-document handler during launch; registration now runs at AppKit's
launch notifications. File-open delivery and the packaged smoke sequence passed
(`bench-results/native-open-fixed.log`). A normally opened fixture window settled
to 0.0% CPU in one `ps` observation; this is not a sustained thermal measurement.

The final attempt to reopen the real Desktop directory stalled in macOS directory
opening. An independent `/bin/ls` subprocess also timed out on that same folder;
the app event thread remained idle and responsive. Its precise OS/filesystem cause
was not established, and no permission settings were changed. Consequently no final
idle-CPU claim is made for the 433-photo folder. The earlier successful corpus and
GUI runs above remain valid. The app was left open without a folder for Cmd+O.

## Supplied external-drive photograph

Source: `DSC00993.ARW` on the user's T7, 49,221,632 bytes. Three usable candidates:

| Preview | Dimensions | Offset | JPEG bytes |
| --- | ---: | ---: | ---: |
| IFD2, selected | 7008×4672 | 606208 | 6,285,152 |
| IFD0 | 1616×1080 | 200866 | 402,636 |
| IFD1 thumbnail | 160×120 | 43966 | 6,715 |

The extracted JPEG was visually inspected and compared byte-for-byte against
exactly that source range. Its byte count and dimensions match the selected
IFD2 preview. The source was opened read-only. No RAW sensor bytes were decoded.

One initial extraction run requested **27,710 metadata/header bytes in 22 logical
reads**, followed by the 6,285,152-byte selected JPEG. Total source bytes requested:
6,312,862, about 12.8% of the ARW. Reads can overlap and OS cache behavior differs
from these application request counts.

| Stage | One observed run |
| --- | ---: |
| File open | 0.103 ms |
| Preview discovery | 1.795 ms |
| JPEG read and marker framing | 16.517 ms |
| Output JPEG write and sync | 36.320 ms |
| Total including output persistence | 54.836 ms |

This is one sample, not a percentile result or a guaranteed cold external-SSD
measurement. Output persistence is a CLI extractor cost and does not belong in
the desktop browsing pipeline. The input range is never read as a full RAW.

## Corpus and protocol

The user confirmed **433 ARWs** as the final local set in
`/Users/tahzib/Desktop/TM/Masai 1`. Filenames were naturally sorted once per run.
Each backend tested all 433 sequentially and then in a seeded random permutation
(`--seed 24301`), without an application image cache. One decoder and pixel buffer
were reused. The selected JPEG is always the largest supported candidate;
dimensions include 7008×4672 and 4608×3072.

No FastCull compilation or second benchmark ran concurrently with the timed
passes. This was an interactive development machine, not an isolated laboratory:
OS caches, clock speeds, other applications, and thermal state were uncontrolled.
The local folder was being populated earlier in the session; the TurboJPEG run
already selected the final 433 files, which the user subsequently confirmed.
Zune ran later on the same confirmed file set and could benefit from warm pages.
There was a brief build of the comparison CLI between backend runs. Compare
decode-stage results as well as totals; repeat in reverse order before making
a final decoder choice.

New runs can use `--decoder both` to snapshot one file list for both backends and
use identical random permutations. That mode runs TurboJPEG before zune and does
not eliminate OS cache or thermal-order bias.

The full native pass reported 433/433 successes in each order. Its sequential
directory scan and natural sort took 2.393 ms. Discovery plus selected-JPEG reads
requested 1,954,107,323 bytes across 433 files, with 9,959 logical read requests
(23 per file). Peak retained compressed buffer capacity was 7,141,579 bytes.
These counts do not include decoder scratch or GPU memory.

## Full-resolution TurboJPEG results

All values are milliseconds; percentiles use nearest rank over successful files.
All 866 attempts succeeded.

| Order / stage | p50 | p95 | p99 |
| --- | ---: | ---: | ---: |
| Sequential discovery | 0.185 | 4.290 | 6.124 |
| Sequential JPEG read + framing | 5.786 | 9.922 | 13.216 |
| Sequential decode | 189.297 | 262.956 | 317.207 |
| Sequential total | 197.605 | 274.918 | 326.125 |
| Random discovery | 0.090 | 0.191 | 0.316 |
| Random JPEG read + framing | 5.741 | 8.425 | 9.923 |
| Random decode | 198.469 | 286.466 | 341.655 |
| Random total | 204.739 | 294.238 | 351.633 |

Sequential throughput was 5.00 photos/s; random was 4.94 photos/s. Full-size
decoding dominates. These numbers do **not** meet the aspirational sub-100 ms
uncached total. More threads alone cannot make one foreground decode faster,
and retaining many 125 MiB pixel buffers would exceed a modest cache budget.

## Full-resolution zune results

All 866 attempts succeeded. Directory enumeration and sort took 1.331 ms.
Application input byte counts and retained compressed capacity matched the
TurboJPEG run exactly. Sequential throughput was 5.04 photos/s, random 4.76.

| Order / stage | p50 | p95 | p99 |
| --- | ---: | ---: | ---: |
| Sequential discovery | 0.081 | 0.118 | 0.215 |
| Sequential JPEG read + framing | 5.228 | 6.267 | 7.043 |
| Sequential decode | 207.994 | 230.858 | 257.531 |
| Sequential total | 213.596 | 237.090 | 265.416 |
| Random discovery | 0.082 | 0.137 | 0.198 |
| Random JPEG read + framing | 5.297 | 7.117 | 9.635 |
| Random decode | 211.299 | 279.884 | 406.257 |
| Random total | 216.960 | 286.980 | 412.837 |

TurboJPEG had lower median latency; zune's first-pass tail was lower, and mean
throughput was close. This is not evidence of a universal decoder winner. Both
remain optional for CLI builds, and ImageIO has not been measured. The desktop
currently uses TurboJPEG with a dedicated foreground lane and one speculative
lane. That implementation choice is not a universal decoder-performance claim
or a UI latency guarantee based on this experiment.

## Reduced decode and resident memory experiment

After the external T7 path became unavailable, repeated runs used the local
`DSC00993.ARW` in the confirmed 433-file set. Each run repeated that file 20 times
in a fresh process, with the same largest 6,285,152-byte JPEG, native TurboJPEG,
and denominators 1, 2, then 4. All 60 attempts succeeded. No warmup observations
were excluded. This is a single-photo, largely warm-cache experiment, not a
corpus percentile or cold-disk comparison. `/usr/bin/time -l` measured the process
peak RSS outside the sandbox because macOS blocks its clock/resource queries
inside the sandbox.

| Scale | Decoded pixels | RGBA MiB | Decode p50 | Pipeline p50 | Pipeline p95 | Peak RSS MiB |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Full | 7008×4672 | 124.90 | 218.702 ms | 226.432 ms | 339.304 ms | 133.40 |
| Half dimensions | 3504×2336 | 31.22 | 173.587 ms | 180.975 ms | 248.864 ms | 38.90 |
| Quarter dimensions | 1752×1168 | 7.81 | 155.299 ms | 162.137 ms | 189.967 ms | 15.16 |

These measurements support scaled pixels as a memory-saving option. They do
**not** establish sub-100 ms uncached presentation, even before GPU upload. The
modest decode reduction despite 16× fewer output pixels suggests substantial
work independent of output size; confirming its exact causes requires profiling.
It would be premature to assume scaling plus an arbitrary worker count solves
the navigation target. Current-photo priority, generation coalescing, decoded
prefetch residency and GPU reuse are now implemented. Their behavior must be
measured separately from these serial single-image runs.

The CLI's full-resolution default is unchanged; `--scale` affects decoded pixels
only. The desktop uses full resolution with its default 512 MiB budget; its explicit
256 MiB setting uses half-width/half-height decoding to leave upload headroom.
Both policies retain the largest embedded JPEG as their source. Viewport changes
never initiate another decode. At 256 MiB, 100% viewing therefore magnifies the
reduced-detail pixels instead of replacing them with a full-resolution bitmap.

```sh
target/x86_64-apple-darwin/release/fastcull-bench \
  '/Users/tahzib/Desktop/TM/Masai 1/DSC00993.ARW' \
  --decoder turbo --scale 2 --passes 20 --order sequential
```

## Validation

At the initial desktop milestone, the all-feature suite passed **81 tests**,
including the desktop modules, and the dependency-free configuration passed
**50 tests**. Clippy with `--all-targets --all-features -- -D warnings` was clean.
That Intel-only release bundle was locally ad hoc signed; older macOS releases
and Intel GPUs were not tested. These are historical validation counts; later
checks are recorded in [filmstrip research](filmstrip-research.md) and
[local-move verification](deleted-workflow.md#verification). The parser smoke
corpus includes 2,048 deterministic mutations; this is not a claim of exhaustive
fuzzing. Input preservation and refusal to overwrite an existing extractor
output are tested. Source-range equality and visual inspection additionally
verified the supplied real photograph's extraction.

Desktop tests cover memory leases/cache eviction, generation cancellation, async
loading, viewport/orientation, XMP byte preservation and atomicity, native exclusive
rename, and I/O coalescing/flush/retry. Mutation tests use temporary synthetic
files only. Read-only EXIF validation across all 433 real ARWs found shutter,
aperture, ISO, and lens metadata in every file with no errors; 427 had orientation
1 and six had orientation 8.

## Reproduction

```sh
cargo build --release --locked --features turbo,zune
target/x86_64-apple-darwin/release/fastcull-bench \
  '/Users/tahzib/Desktop/TM/Masai 1' --limit 500 --order both --decoder both
```

Raw outputs are retained locally in ignored `bench-results/`; photographs are
not committed as test fixtures. Automated tests instead use tiny synthetic
TIFF/JPEG data, nine observed A7 V metadata layouts, and a sparse multi-gigabyte
reader that fails on any RAW sensor payload access.
# Workflow feature verification, 2026-09-21

Zoom lock, temporary 100% peek, optional rating auto-advance, last-session
restore, pinned comparison, and rating/reject undo were checked on the Intel
Mac used for the earlier measurements. No dependencies were added.

- `cargo test --locked --all-features --target x86_64-apple-darwin`: **114 passed**.
- All-target/all-feature Clippy with warnings denied and formatting checks passed.
- Four read-only desktop smoke runs each produced **33 frames**: the five-photo
  synthetic rating fixture, the user's 433-file folder at 512 MiB, and APFS clones
  of six 7008×4672-preview ARWs at both 512 and 256 MiB. These runs exercise a
  selection of photographs, not every decode in the 433-file corpus.
- All 33 synthetic frames passed pixel checks: no red-biased pixels among
  107,015,680 pixels, correctly separated reference/current panes, a neutral
  divider, and correct 8×8 fixture pixels during 100% peek. Intentional UI text
  colors were accounted for separately. Real comparison frames were visually
  inspected as well.
- Original 433 ARW sizes/mtimes remained unchanged. Synthetic source/sidecar
  hashes remained unchanged, with no source ARW/XMP additions or removals.
- The packaged executable was verified as x86_64-only and its ad hoc signature
  passed verification.

Logs and the exact executable hash are in `bench-results/workflow-validation/`;
frame checks are in `bench-results/workflow-frame-inspection.json`. These are
functional checks, **not controlled navigation benchmarks**: frame readback,
filesystem caching, and active background applications disturb timings.

Comparison retains an existing full-resolution texture (about 125 MiB for
7008×4672) in the shared budget. At 512 MiB it may give up GPU neighbor prefetch
and release the outgoing main texture before upload; it does not promise the
same cache hit rate as single-image viewing. Zoom lock/peek operate only on view
transforms. Session writes are coalesced and rating/undo writes remain off-UI.
