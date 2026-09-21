# FastCull desktop architecture

FastCull supports Intel macOS (`x86_64-apple-darwin`) only. Its viewing path is
TIFF metadata → largest embedded JPEG → TurboJPEG RGBA → Metal texture.
RAW sensor development is never part of navigation.

The desktop implementation now includes folder browsing, asynchronous loading,
directional prefetch, CPU/GPU caches, viewport transforms, ratings/XMP, local file moves, EXIF,
an optional filmstrip, native menus/open-document handling, and app packaging.
The extractor and serial benchmark remain separate entrypoints. [Measured CLI results](measurements.md)
are not desktop-navigation measurements; GUI smoke evidence is recorded separately.
The [Intel audit](intel-audit.md) documents measured CPU/GPU choices and their limits.

## Exact direct dependencies

| Dependency | Pin | Purpose |
| --- | --- | --- |
| `turbojpeg` | `=1.5.1` | `turbo`; bundled libjpeg-turbo, CMake and required Intel SIMD; desktop decoder |
| `zune-jpeg` | `=0.5.15` | Optional `zune`; pure Rust benchmark comparison |
| `zune-core` | `=0.5.1` | Optional zune decoder options |
| `winit` | `=0.30.13` | Desktop event loop/window/input, `rwh_06` |
| `wgpu` | `=24.0.5` | Defaults disabled; Metal and WGSL only |
| `pollster` | `=0.4.0` | GPU initialization and worker-side completion setup |
| `rfd` | `=0.15.4` | Native folder picker and confirmation/error dialogs |
| `font8x8` | `=0.3.1` | Small bitmap status/message text |
| `quick-xml` | `=0.37.5` | Validate XMP while preserving original byte spans |
| `muda` | `=0.16.1` | Native macOS menus and accelerators |
| `trash` | `=5.2.3` | Explicit macOS Trash operation via NSFileManager |
| `cc` | `=1.4.7` | Desktop build dependency for the small Objective-C event bridge |

Desktop dependencies are optional behind `desktop`; `desktop` enables `turbo`.
The default extraction build needs no third-party runtime crates. Cargo.lock
pins the resolved graph. CMake/NASM require SIMD support in the bundled
libjpeg-turbo build; missing assembler support fails the build. At runtime the
library selects supported SIMD routines, with scalar paths where needed.
No Tokio, Rayon, general-purpose image crate,
database, or whole-file mmap dependency is used. Scheduling uses `std` threads,
mutexes, condition variables, atomics, and Winit event-loop wakeups.

The configured deployment floor is macOS 10.15. The native APIs used include
Cocoa document-open callbacks and `renamex_np(RENAME_EXCL)` (macOS 10.12+).
Unsafe code is denied except the small documented platform FFI boundary.
Compilation for a deployment floor does not establish runtime coverage of every
older Intel GPU/OS combination.

## Preview extraction and decoding

`PreviewReader` holds one file open through discovery and extraction. It reads
classic TIFF in either byte order and follows next-IFD, SubIFD, Exif, and
interoperability pointers. It pairs JPEGInterchangeFormat/Length in each IFD,
handles supported single-strip display JPEGs, and recognizes documented Sony
MakerNote preview blobs. CFA/RAW strips are excluded before payload inspection,
even when TIFF Compression is 7.

Sony fallback handling recognizes the `SONY DSC`/`SONY CAM` prefixes and tag
`0x2001`'s 32-byte preview prefix. It does not generalize that layout to every
Sony model. A7 V evidence in [sample-layouts.md](sample-layouts.md) demonstrates
why following IFD2 matters: IFD0 can hold only an HD preview.

Candidates are bounded against file size, inspected through JPEG SOF headers,
and ordered by verified pixel area, then compressed length. Sensor IFD dimensions
do not determine preview size. Discovery reads metadata and candidate headers/
tails; extraction seeks directly to the selected JPEG and checks marker framing.
If a candidate fails, another metadata-declared supported JPEG can be attempted.
No blind search through RAW sensor bytes is performed.

Safety limits include 128 IFD visits, 4,096 entries per IFD, bounded pointer and
candidate lists, 4 MiB of metadata reads, 128 MiB per compressed preview,
16,384-pixel decoded sides, and 256 MiB of RGBA output. Checked arithmetic and
file-range checks precede reads/allocations. Unsupported BigTIFF, absent previews,
non-DCT/12-bit JPEGs, or unsupported strip/tile layouts report errors. JPEG
framing does not prove entropy validity; the decoder performs the latter check.

The desktop uses TurboJPEG's accurate IDCT and smooth chroma upsampling, producing
tightly packed RGBA8 directly. It does not convert through an intermediate RGB
bitmap. Each decode worker retains a decompressor but transfers completed pixel
ownership into the cache; hidden large buffers cannot remain in idle decoders
outside accounting. Compressed JPEG buffers are released after decode rather
than retained in a second speculative cache.

The serial CLI also compares zune and supports TurboJPEG `--scale 2|4|8` reduced
IDCT. Those experiments keep the largest JPEG and reduce output dimensions;
ImageIO remains an unmeasured alternative. CLI buffer reuse and timings are
separate from the desktop ownership/scheduling behavior.

## Threading, navigation, and prefetch

The UI thread owns keyboard/mouse events, current photo identity, viewport,
status, native menus, and render submission. Navigation performs no file reads,
TIFF/EXIF/XMP parsing, JPEG decoding, or image upload waits. It normally uses
`ControlFlow::Wait`, requesting redraws when input or completed work changes the
view. Native dialogs are explicit modal UI operations.

The worker structure is deliberately small:

- A directory scanner runs for an open request and cancels on a newer folder
  generation. It enumerates one directory and naturally sorts filenames.
- One dedicated foreground decoder services the latest requested photograph.
- One speculative decoder services useful neighbors after foreground/upload
  headroom is available.
- One upload worker transfers prepared images and waits for GPU completion;
  pending requests are replaceable. It can preupload the nearest useful neighbor.
  Main-photo requests outrank its optional, single queued thumbnail upload.
- One utility-priority thumbnail decoder fills the optional visible filmstrip
  after foreground work settles. Its latest plan prioritizes up to 24 visible
  cells, then up to 12 neighbors on each side, for at most 48 requests, with
  one decoded result awaiting upload/acknowledgment. It accepts only embedded
  JPEGs up to 512 pixels per side and 256 KiB compressed, preferring the smallest;
  native reduced-IDCT decoding bounds output to 192 pixels per side. The corpus's
  160×120 Sony thumbnail is decoded unchanged. Missing or
  unavailable thumbnails remain placeholders without a large-preview fallback.
- One serialized I/O worker handles XMP saves, local `deleted`-folder moves and
  restores, bounded EXIF/XMP reads, and explicit rejected-photo/Trash batches.
  Pending logical edits outrank optional reads and
  remain FIFO so every edit can retain its authoritative previous rating.
- One session worker reads the last-viewed path and preferences, then coalesces
  tiny state writes (500 ms trailing delay, at most 2 seconds while changing).
  It sleeps between changes and is not constructed for read-only smoke runs.

The default interest window is eight images ahead and three behind. It reverses
when navigation reverses. These are scheduling priorities, not promises to retain
11 full-resolution decoded images. The nearest useful GPU neighbor is prioritized;
further decoded residency depends on the main-photo pool's byte budget.

Requests carry folder/session, index, and navigation generation. Rapid arrows
replace the requested target rather than enqueueing every intermediate frame.
Workers check relevance between parsing, JPEG reading, decoding, and publication.
A native JPEG call already executing cannot be interrupted; its stale result is
discarded unless still useful. Completions do not move the user's requested index.
Loader notifications carry identities, not ownership of additional pixel buffers.

The filmstrip owns a separate visible range. Scrolling over it changes that range
without navigating or decoding a main photo. Clicking a thumbnail selects it;
normal arrow navigation still selects main photos. Explicit selection
reveals the selected thumbnail only if it falls beyond a visible edge, keeping
the range stable while navigating within it. Metadata/status completions cannot
recenter a range the user has scrolled elsewhere.

Long scans may publish a provisional sorted batch once at least 32 ARWs have been
found and 40 ms have elapsed. Final ordering preserves an intentional navigation
selection; otherwise it selects the requested ARW or naturally first filename.
A new loader session prevents earlier index-based results from being
mistaken for a different photo after a list changes. There is no recursive scan,
file hashing, or catalog construction.

## Shared memory accounting and GPU ownership

`--cache-mb` selects 256, 512 (default), 1024, or 2048 MiB. `MemoryLease` reservations
follow image ownership across workers, cache entries, uploads, and GPU textures.
Each allocation is counted once despite multiple `Arc` references. JPEG and RGBA
buffer capacities, GPU image texels, and conservative row-padded upload staging
count toward the configured total. A fixed 8 MiB pool is carved out for thumbnail
allocations, leaving 504 MiB for main photos at the default 512 MiB setting.
The pool is a reservation limit, not an eager allocation. Main-photo and thumbnail
leases use their respective pools; their limits sum to the configured total.
The decoded main-photo cache has a soft target of half its pool, subject
to foreground reservations and higher-priority neighbors.

A 7008×4672 RGBA image occupies 130,965,504 bytes, about 125 MiB. During navigation,
old display texture, new CPU pixels, new texture, and staging can coexist. GPU
cache entries and speculation are evicted to make room; under pressure even the
old display may be released rather than exceed managed allocations. Priority and
available bytes control actual residency, including the nearest GPU neighbor.

512 MiB and larger settings decode at full preview resolution. The 256 MiB mode
uses TurboJPEG half-width/half-height output so upload transitions fit. Geometry
continues to use original preview dimensions; at 100% this smaller representation
is magnified and has less detail. Zoom, pan, fit/fill, orientation, and resizing
only update texture transforms and do not initiate another decode.

Zoom retention is enabled by default. `L` toggles and saves the preference;
legacy session records migrate to enabled once. Custom and 100% views retain
scale and normalized focus across images, clamped to valid image edges. New
dimensions and known EXIF orientation apply in one transition so a temporary
unrotated shape cannot discard the focal point. Fit and Fill retain their modes.

Large uploads prepare an explicitly mapped staging buffer on the upload worker,
then submit a buffer-to-texture copy. Completion is polled without holding a
blocking device wait across foreground submissions. Idle memory waits sleep
until memory or requests change. Upload staging reservations remain until GPU
completion. Submitted render work
retains image ownership until its completion callback. A remaining external
`Arc` therefore prevents eviction from pretending that storage was freed. Native
Metal allocations can still have alignment/driver overhead beyond texel estimates.

This budget is **not** a whole-process RSS cap. Native decoder scratch/bookkeeping,
driver caches, presentation surfaces, UI data, path indexes, and allocator overhead
are outside it. Measure RSS and GPU behavior alongside managed-memory logs,
especially on integrated Intel GPUs sharing physical RAM with the CPU.

Filmstrip JPEG/RGBA buffers, textures, and upload staging use the reserved 8 MiB
pool. A rolling cache retains at most 48 GPU thumbnails and 4 MiB of texels,
protecting visible cells and evicting distant entries first. Up to 12 neighboring
thumbnails on either side are prefetched after visible cells. The 192-pixel
output bound lets all 24 visible cells fit under the texture cap. Overlapping
entries survive main-photo navigation and strip scrolling. Main-image memory
pressure cannot evict the thumbnail pool, preventing the strip from clearing
and refilling on every photograph. Hiding the strip cancels
queued work and releases retained thumbnail handles; frame callbacks keep leases
alive only until submitted use completes. Selection-only updates reuse existing
GPU state; unchanged transforms skip uniform writes. A bounded caption cache
reuses labels as photos move between cells. The strip adds no dependencies and no idle
redraw loop. Component benchmark results and the integrated-validation status
are kept separate in [filmstrip-research.md](filmstrip-research.md).

## XMP, local moves, and Trash

Space/X move the displayed main photo and any existing matching XMP into its
directory's `deleted` subfolder. They preserve RAW/XMP bytes and existing ratings;
they do not assign `Rating=-1`. S/D are incremental zoom-in/out aliases and can
repeat, while Space/X act once per physical press. Shift+Space remains previous
photo. The A preference controls only auto-advance after rating edits; successful
local moves always select the next surviving photo, or the previous one at the end.

Move requests capture the main-photo path, independent of the filmstrip's scroll
range or the pinned reference. Filesystem work stays on the serialized I/O worker
so preceding rating saves finish before their sidecar moves. The UI removes a
successfully moved path from its active list and invalidates index-based loading
sessions. Existing destinations are not overwritten. Cmd+Z restores a local move
with its sidecar in the current session; U/0 only clears rating metadata.

File → Open deleted Folder uses the same app to review moved photos. Local moves
from a `deleted` folder are refused to avoid nesting. The batch local-move command
collects existing XMP rejects and asks for confirmation. These local moves are
separate from the existing macOS Trash workflow described below.
The [local-move workflow](deleted-workflow.md) records collision/rollback handling,
current-sidecar restoration, cache preservation, and verification evidence.

The UI applies a rating optimistically and assigns a monotonically increasing
revision. The I/O worker serializes logical edits in FIFO order. Completion updates saved
state only for the matching revision, preventing an older success from marking a
newer edit saved. Pending and failed saves remain visible. Quit requests a flush;
failures offer an explicit retry, quitting without those changes, or cancellation.
The worker also drains queued writes during normal destruction.

Each successful edit retains its previous `Option<i8>` from the same validated
XMP snapshot used for replacement. The worker holds up to 100 undo records; queued
undo can follow an edit before its save finishes. Undo checks the expected current
rating before restoring the old value, removes only the property when originally
absent, and preserves unrelated external changes. Failed undo stays retryable
without becoming an unsaved normal edit. UI interaction tokens prevent delayed
undo completions from stealing selection after navigation or another rating.

`-1` is rejected and `0..5` are clear/stars in a sibling `.xmp`. Parsing resolves
namespace URIs, not fixed prefixes, and supports rating attributes or simple text/
CDATA elements. UTF-8 BOMs, entities, and unrelated XML bytes are preserved.
Only the rating value is replaced; absent ratings get a locally namespaced
attribute on a suitable RDF Description. No whole-document serializer is used.

Malformed XML, ambiguous duplicate properties/subjects, unsupported encodings or
complex rating forms, DTDs, invalid values, excessive nesting, read-only sidecars,
symlinks, and files over 8 MiB fail safely. Source ARWs are never written. A unique
sibling temporary is created exclusively, written, given existing permissions,
and synced. New sidecars publish with macOS `renamex_np(RENAME_EXCL)`, which cannot
overwrite a concurrently created destination and does not require hard links.
Existing replacements compare bytes, inode, size, timestamps, and mode immediately
before atomic rename. This is a **best-effort external-edit guard**: an unrelated
writer can still race the final existing-file check/rename. Conflicts are surfaced,
not silently retried. Directory sync is best effort where supported.

Rejected-photo collection reads durable sidecar states after pending writes.
Unsaved failures block that workflow. The UI shows an explicit count confirmation;
the worker rechecks rejection and a regular ARW source before using macOS Trash.
Only RAWs move; sidecars remain. App state prevents quit/open/navigation/ratings
from racing an active Trash workflow. No permanent-delete path exists.

The separate EXIF reader visits only IFD0/ExifIFD and requested bounded values,
with a 256 KiB read budget, 16-directory limit, and short string limits. It loads
shutter, aperture, ISO, focal length, camera/lens, date, and orientation; invalid or
missing orientation defaults to 1. EXIF completion can rotate the existing texture
without affecting decode. Real-corpus read-only checks found all requested exposure
fields in 433 files and six images with orientation 8.

## Native app behavior and verification

Winit handles window input/file drops; the small Objective-C bridge handles Finder/
Dock open-document events. Muda supplies native menus and Cmd accelerators. The
bundle script generates an icon, checks the Intel architecture, creates
`dist/FastCull.app`, and applies a local ad hoc signature. It does not notarize or
install a distribution build.

`FASTCULL_LOG=1` records parse/read/decode, CPU/GPU/prefetch hits, GPU submission
and completion, request-to-first-present-submission, and managed bytes. The last
metric ends at presentation submission, not actual panel scanout. The read-only
`--smoke-test` exercises navigation, coalesced skips, fit/100%, pan, resize,
fullscreen, filters, locked zoom, temporary peek, and pinned comparison while
capturing only FastCull's own frames. Capture readback disturbs timings.

Tests cover parser bounds and sparse real layouts, navigator/generation behavior,
lease/cache ownership, asynchronous loading, viewport orientation, XMP preservation,
atomic publication/conflicts, FIFO edit/undo/flush/retry, and session-state saves. Filesystem
mutation tests use temporary synthetic photographs/sidecars. Real-photo checks and
GUI smoke runs remain distinct from exhaustive fuzzing or a 2,000/10,000-photo soak.

The filmstrip revision verified on 2026-09-21, before the current local-move and
S/D-shortcut changes, passed 148 tests, strict Clippy,
and formatting checks. Its 45-frame smoke passed on the synthetic fixture, the
433-photo A7 V folder at 512 MiB, and a six-photo 7008×4672 fixture at both 256
and 512 MiB. The real-folder run checked 3,773 cached texture identities; comparing
its end/start strip captures found all 3,001,920 main-canvas pixels unchanged while
the strip contents changed. Each six-photo run decoded six thumbnails during the
first strip opening despite subsequent main-photo navigation, and decoded them
again only after the explicit hide/reopen. These are functional cache/selection
checks, not controlled latency measurements. See [filmstrip verification](filmstrip-research.md#integrated-functional-verification)
for the evidence paths and earlier-build results.
The later local-move and S/D-shortcut revision passed 180 tests, strict Clippy,
and formatting checks. Its [desktop recheck and packaging evidence](deleted-workflow.md#verification)
are tracked separately.

Intel bottlenecks remain JPEG entropy/IDCT, memory bandwidth, upload copies and
synchronization, external-drive latency, and thermal load from speculation. The
current dedicated foreground capacity and one speculative lane limit CPU pressure;
more workers, mmap, ImageIO, and larger caches require new measurements. Cached
navigation under 16 ms and uncached first display under 100 ms are goals, not
blanket guarantees. Final GUI measurements are maintained in [measurements.md](measurements.md).
