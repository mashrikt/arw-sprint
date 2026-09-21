# Filmstrip research: source facts and measurement limits

These are the source facts behind the component research, not a claim about
integrated GUI performance. Sources were checked on 2026-09-21. The subsequent
filmstrip implementation uses the bounded small embedded thumbnail; the medium
preview remains an unimplemented quality alternative. See the
[implementation status and measurements](filmstrip-research.md).

## Official decoder guidance

- The [libjpeg-turbo README](https://github.com/libjpeg-turbo/libjpeg-turbo/blob/main/README.md)
  documents reduced-IDCT scaling, including 1/8, 1/4, and 1/2. Its qualification
  is: “only 1/4 and 1/2 are SIMD-accelerated.” Do not assume eighth-size output
  is always faster than quarter-size output on Intel. The project's vendored
  library is 3.1.0, and its [versioned README](https://github.com/libjpeg-turbo/libjpeg-turbo/blob/3.1.0/README.md)
  has the same qualification.
- The [official TurboJPEG API documentation](https://rawcdn.githack.com/libjpeg-turbo/libjpeg-turbo/main/doc/turbojpeg/group___turbo_j_p_e_g.html)
  explains: “Decompression scaling is a function of the IDCT algorithm.”
  Scaling reduces destination dimensions; the API still takes a compressed
  JPEG buffer. Our wrapper supplies the complete selected JPEG. Selecting a
  smaller embedded JPEG saves input bytes as well as output pixels; merely
  scaling the largest JPEG does not provide that input reduction.

## Official macOS scheduling guidance

- [Apple's Mac QoS guide](https://developer.apple.com/library/archive/documentation/Performance/Conceptual/power_efficiency_guidelines_osx/PrioritizeWorkAtTheTaskLevel.html)
  says the system uses QoS to adjust “scheduling, CPU and I/O throughput, and
  timer latency.” It documents `pthread_set_qos_class_self_np` and cautions
  against priority inversions. A Rust worker can use the native pthread API;
  adopting an additional dispatch abstraction is not required for QoS alone.
- [Apple's background-priority documentation](https://developer.apple.com/documentation/dispatch/dispatch_queue_priority_background?language=objc)
  states that “any disk I/O is throttled to minimize the impact on the system.”
  Use QoS constants rather than the legacy dispatch priority constant shown
  on that page. Utility QoS for visible thumbnail filling, and background QoS
  for expendable speculation, are experiments to evaluate rather than proven
  fastest settings. QoS does not cancel an in-progress decoder call or undo
  submitted GPU work.
- [Apple's I/O guide](https://developer.apple.com/library/archive/documentation/Performance/Conceptual/power_efficiency_guidelines_osx/MinimizingIO.html)
  recommends minimizing unnecessary access and considering asynchronous I/O
  for significant data volumes. The existing bounded TIFF reads and direct
  JPEG range reads fit this goal. A thumbnail strip is not a reason to read
  complete ARWs or create a persistent thumbnail database.

## Preview quality and orientation evidence

The [recorded local layouts](sample-layouts.md) have 160×120 thumbnails,
1616×1080 medium previews, and 3:2 main previews. Structural metadata alone
does not establish whether the 4:3 thumbnail is cropped, stretched, or padded.

During this research, **one** actual local A7 V image's 6,715-byte 160×120
thumbnail and 402,636-byte 1616×1080 preview were extracted and visually
compared. The small thumbnail retained the same composition with black bars
approximately six pixels high at the top and bottom. Fine animal detail was
very small. This verifies that pair only; it does not establish identical
padding or composition across the 433-file folder, capture modes, or firmware.

The tiny JPEG is the implemented performance choice. Preserve its aspect ratio;
do not automatically remove bars based only on dimensions. At 2× display
scale, 160 physical pixels cover only 80 logical points without upscaling.
If a sharper strip proves necessary, quarter-size medium decoding produces
404×270 pixels (426 KiB RGBA); eighth-size produces 202×135 (107 KiB).

Thumbnail JPEGs need not contain orientation metadata. The implementation reads
the bounded TIFF orientation during discovery and applies it once in rendering.
It does not depend on a complete EXIF read or assume landscape.

## Benchmark interpretation

The CPU contention harness alternates ABBA and BAAB rounds with ordinary thread
scheduling and no excluded warmup. Main-decoder construction occurs outside
per-request timing, while worker startup can overlap the first concurrent
request. It reports retained-cache, in-flight RGBA, and JPEG-capacity peaks
separately; the 16 MiB cap applies to retained thumbnail pixels, not total
process memory. Completed/discarded jobs and cache hits indicate work performed.
The readiness counter records windows fully cached before the next foreground
request; it does not measure milliseconds from navigation to a populated strip.

- Interleave baseline and thumbnail modes in balanced ABBA blocks, including
  both block starting orders. Report each pair's effect, not only pooled
  percentiles. Record warmup, startup, OS-cache, and thermal conditions.
- Measure contention with foreground navigation as well as isolated thumbnail
  throughput. Keep queue depth, active workers, and retained thumbnail bytes
  bounded. Report dropped/canceled work and work still running at measurement
  end; otherwise cheap-looking results can hide incomplete work.
- Account for compressed bytes, CPU pixels, upload staging, and GPU textures.
  One 7008×4672 RGBA main frame is 124.90 MiB. A small thumbnail allocation can
  cross a cache threshold and evict a whole main neighbor. Prefer reclaiming
  thumbnails to evicting main-photo neighbors.
- Separate first-use costs from steady-state samples. Exclude diagnostic
  captures and unrelated corpus reads from timed runs. A headless CPU/GPU
  experiment does not measure actual UI input latency or display scanout.
- Existing short GUI smoke runs are functional evidence, not a measured p95
  navigation baseline. The requested 16 ms responsiveness remains an aspiration
  until a sufficiently sampled end-to-end test establishes it.
