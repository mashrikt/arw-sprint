# RAW viewing and additional formats

Sony A7 V ARW remains the primary format. Ordinary browsing still locates and
reads embedded JPEGs, uses TurboJPEG on background threads and uploads to Metal.
Standalone JPG/JPEG files use the same bounded decoder. TIFF-based DNGs use their
embedded JPEG when possible. Files without a usable preview can try the RAW path.
The scanner skips hidden/AppleDouble and recognized temporary companions; it does
not discard legitimate photos just because their name contains "cache".

## Explicit RAW mode

R or the bottom-right View RAW button renders only the current photo. Navigating
leaves that single-photo mode. Shift+R keeps RAW browsing enabled and prepares one
neighbor in the navigation direction. Both are customizable. JPEG originals do
not offer RAW conversion. Brightness, zoom and pan still operate on GPU textures.

The RAW decoder is a pinned, statically linked LibRaw snapshot with Sony A7 V
support. It uses full-resolution PPG demosaicing, camera white balance, sRGB output
and the camera's active crop. It produces RGBA8 display pixels, not a developed
16-bit export. No denoising, sharpening or lens correction is applied. Color and
brightness need not match the camera JPEG or an editor's rendering.

One dedicated worker coalesces requests to the latest wanted path and checks
cancellation in native processing. Completed results also carry session and job
identities. RAW GPU uploads share the existing upload worker, because overlapping
wgpu device error scopes across workers would be unsafe. Preview uploads retain
priority. Native scratch is released after each decode; idle workers sleep.

The usual 512 MiB managed budget still covers JPEG buffers, decoded pixels,
textures and upload staging. Explicit RAW decoding additionally permits up to
384 MiB of estimated native scratch. Native library/framework overhead is not a
strict process-RSS guarantee. RAW cache holds at most the current photo and one
neighbor. A memory-limit failure leaves the current preview visible. Larger
sensors or unsupported encodings can exceed these limits.

Before sensor decoding, the native bridge validates dimensions and output sizes.
TIFF/EXIF/MakerNote hooks reject individual declared metadata values above 8 MiB,
more than 16 MiB of metadata in total, or more than 16,384 metadata entries. The
native ICC profile limit is 4 MiB. LibRaw also has decoder-specific allocation
checks. These guards and the scratch estimate reduce malformed-file risk; they
are not a process sandbox or a strict operating-system memory limit.

## Format boundaries

- Sony A7 V lossless files were checked locally. Other ARW compression modes
  remain unverified even though the pinned decoder contains newer Sony support.
- DNGs can vary in container, compression and sensor layout. Classic TIFF JPEG
  previews and a synthetic uncompressed Bayer DNG were tested. Lossy JPEG DNG
  sensor decoding, BigTIFF and every camera-specific DNG variant are not promised.
- Standalone JPEG EXIF orientation and bounded APP1/APP2 parsing are supported.
  Embedded ICC profiles and explicit EXIF Adobe RGB are converted to sRGB on a
  worker through ColorSync. Existing ARW/DNG embedded-preview color behavior is
  unchanged. Malformed or unsupported profiles report an error.
- ARW sidecars keep the existing `photo.xmp` convention. JPEG/DNG sidecars include
  the original extension (`photo.JPG.xmp`, `photo.DNG.xmp`). This avoids collisions
  in mixed folders but is not universally interoperable with other photo tools.
  Moves apply only to the selected original and its own sidecar, without grouping
  RAW+JPEG pairs. Tests use generated files, never the user's photographs.

## Local measurements

Host: Intel Core i7-1068NG7, four cores, 32 GB RAM, Iris Plus graphics.
Initial native-engine checks on three Sony A7 V lossless ARWs measured approximately
2.3 to 2.5 seconds per full-resolution render with warm filesystem data. PPG was
about 2.5 seconds versus 5.1 seconds for AHD in the same probe. The final bridge
honors the camera crop and produced 7008x4672 pixels on DSC00993.ARW in about 2.5
seconds. Standalone decoder peak process RSS was about 448 MiB; the complete app
also retains textures and framework allocations. These are local observations,
not a latency guarantee. Preview browsing does not invoke this decoder.

Repeat the bridge benchmark with:

```sh
cargo run --release --features desktop --bin fastcull-raw-bench -- \
  --repeat 3 --cancel-test /path/to/photo.ARW
```

The diagnostic reports dimensions, probe/decode percentiles and scratch estimates.
It has a wider native diagnostic limit than the desktop's 384 MiB scratch limit.
`fastcull --raw-smoke-test NEW_OUTPUT_FOLDER /path/to/photos` exercises source
switching, zoom retention, neighbor caching and cancellation without changing
ratings, photos or preferences. Captured frames remain local in the supplied folder.

The vendored LibRaw copyright, license and source revision are in
[`vendor/libraw`](../vendor/libraw/README.arw-sprint.md), with notices in the app bundle.

## Release validation, 9 November 2026

- 292 automated tests passed, including cancellation, corrupt input, metadata
  allocation limits, mixed-format sidecar ownership, moves/undo and color handling.
  Formatting and Clippy checks passed.
- The packaged Intel app passed normal navigation, filmstrip, zoom, brightness,
  filters, comparison and EXIF checks in a folder of 433 Sony A7 V ARWs.
- RAW window checks passed for source switching with zoom lock disabled, a pinned
  RAW reference, immediate cached-neighbor navigation, metadata loading and
  cancellation during navigation. JPEG previews remain the default.
- Generated fixtures passed automatic RAW fallback for a preview-free Bayer DNG,
  EXIF orientation, standalone JPEG display, bounded JPEG thumbnails and junk-file
  filtering. This is synthetic coverage, not a claim of broad real-camera DNG support.
- The final bridge benchmark rendered DSC00993.ARW twice in 2.22 and 2.44 seconds,
  with 0.70–0.74 ms metadata probes. A deliberately cancelled decode stopped in
  97 ms and the following decode succeeded. These are two local samples.
- All 464 files in the original test folder, including 433 ARWs, retained their
  sizes and modification times. File-move tests used disposable fixtures only.

In a rapid RAW-to-preview-to-RAW transition, the RAW worker permits up to one
second for existing preview allocations to retire. This wait is cancellable and
never runs on the UI or GPU-upload thread.
