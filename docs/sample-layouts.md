# Sony A7 V preview layout evidence

The A7 V files inspected for this project contain three JPEG previews. The
largest is in **IFD2**, reached through the next-IFD links. The JPEG referenced
by IFD0 is only 1616 × 1080. Stopping at IFD0 would discard most of the detail
needed for culling.

These files are classic little-endian TIFF (`II`, magic 42). Follow the TIFF
header's IFD pointer, each next-IFD pointer, and SubIFDs; do not hardcode the
observed offsets. The tables below are validation evidence, not format rules.

## Private local camera originals

Three representative ILCE-7M5 originals were inspected on 2026-09-21 using
bounded `seek`/`read` operations. The reference inspection read 1,800 bytes per
file: directory records, selected numeric fields, JPEG headers through SOF, and
the two boundary markers. It did not read the RAW sensor payload or decode it.
This byte count describes the reference inspection, not the Rust benchmark.

| Anonymous sample | File bytes | IFD2 JPEG offset | JPEG bytes | JPEG dimensions |
| --- | ---: | ---: | ---: | --- |
| private-local-1 | 19,615,744 | 462,848 | 1,170,107 | 4608 × 3072 |
| private-local-2 | 40,611,840 | 487,424 | 2,496,810 | 7008 × 4672 |
| private-local-3 | 46,649,344 | 389,120 | 4,513,637 | 7008 × 4672 |

All three previews use 8-bit, three-component baseline JPEG (SOF0), start with
`FF D8`, and end exactly at `FF D9`. IFD1 holds a 160 × 120 thumbnail. IFD0 starts
at 8, IFD2 at 145,074, and the RAW SubIFD at 145,382 in these examples. The
MakerNote starts directly with its IFD count at 5,222, without a `SONY` header.

[The local layout fixture](../tests/fixtures/sony-a7v-local-layouts.json) keeps
only camera model, sizes, offsets, structural tags, and JPEG dimensions. It
contains no photographs, filenames, paths, serial numbers, capture timestamps,
GPS, or other identifying EXIF. Private source photographs are not distributed
or licensed by this repository. Structural fixture facts are not JPEG images;
they cannot validate entropy decoding or displayed image quality by themselves.

## Public CC0 reference metadata

The [raw.pixls.us repository](https://raw.pixls.us/) lists six A7 V samples under
[CC0](https://creativecommons.org/publicdomain/zero/1.0/). Only their published
metadata listings were fetched; the original public ARWs were not downloaded
or decoded. The repository's mode labels are retained without assuming that
RAW compression determines preview placement.

| Sample | Mode | IFD2 JPEG offset | JPEG bytes | IFD2 dimensions |
| --- | --- | ---: | ---: | --- |
| [8845](https://raw.pixls.us/getfile.php/8845/exif/full_compressed_lossless.ARW.exif.txt) | Full frame, lossless compressed | 548,864 | 4,734,564 | 7008 × 4672 |
| [8846](https://raw.pixls.us/getfile.php/8846/exif/full_compressed_HQ.ARW.exif.txt) | Full frame, compressed HQ | 536,576 | 4,259,249 | 7008 × 4672 |
| [8847](https://raw.pixls.us/getfile.php/8847/exif/full_compressed.ARW.exif.txt) | Full frame, compressed | 536,576 | 4,270,440 | 7008 × 4672 |
| [8848](https://raw.pixls.us/getfile.php/8848/exif/apsc_compressed_lossless.ARW.exif.txt) | APS-C, lossless compressed | 643,072 | 2,359,452 | 4608 × 3072 |
| [8849](https://raw.pixls.us/getfile.php/8849/exif/apcs_compressed_hq.ARW.exif.txt) | APS-C, compressed HQ | 634,880 | 2,312,397 | 4608 × 3072 |
| [8850](https://raw.pixls.us/getfile.php/8850/exif/apsc_compressed.ARW.exif.txt) | APS-C, compressed | 655,360 | 2,555,667 | 4608 × 3072 |

[The public layout fixture](../tests/fixtures/sony-a7v-public-layouts.json)
records each source URL, its published original-file SHA-256, licensing,
retrieval date, and verification limits. Width and height in these public
fixtures come from metadata, not independently parsed JPEG SOF markers.

## Parser consequences

- Pair `JPEGInterchangeFormat` (`0x0201`) with
  `JPEGInterchangeFormatLength` (`0x0202`) within the same directory. Compare
  candidates by verified JPEG pixel area; compressed byte length is only a
  secondary ordering criterion.
- A RAW SubIFD may also say `Compression = 7` (JPEG). The inspected lossless
  files have CFA photometric interpretation (`32803`), one sample per pixel,
  and 14-bit samples. Those strips are sensor data, not display previews.
  Only consider strip-based fallback candidates with appropriate display-image
  photometric/sample metadata and a supported lossy JPEG header.
- Obtain candidate dimensions from the JPEG SOF header. The padded sensor
  dimensions in a RAW SubIFD are not the preview dimensions. SOF3 lossless
  sensor JPEG must not win preview selection because it has a larger area.
- Deduplicate offset/length pairs; retain valid candidates when an unrelated
  optional directory or candidate is malformed. Bound traversal depth, IFD
  count, entries, offsets, header reads, preview bytes, and decoded dimensions.
- Seek over JPEG APP segments using their declared, checked lengths. Do not
  search RAW bytes for `FF D8`. Boundary checks alone do not prove that JPEG
  entropy data is complete; the JPEG decoder remains responsible for that.

ExifTool names `0x0201` / `0x0202` in ARW IFD0 as PreviewImageStart/Length.
Its naming depends on the directory and format; the underlying tag IDs are
the useful facts for this reader.
[ExifTool EXIF tags](https://exiftool.org/TagNames/EXIF.html)

For older or alternate Sony layouts, MakerNote tag `0x2001` is a byte blob with
a 32-byte proprietary header before its JPEG. Some models carry an empty tag.
ARW examples have a normal SOI; certain camera JPEG MakerNote variants need
their first JPEG byte repaired by ExifTool, which should not be generalized to
arbitrary ARW data. A fallback should only accept the documented placement
and validate the candidate.
[ExifTool Sony parser](https://github.com/exiftool/exiftool/blob/master/lib/Image/ExifTool/Sony.pm)

Sony MakerNotes have multiple layouts. Signature forms such as `SONY DSC` and
`SONY CAM` place the IFD 12 bytes into the value; the ARW/SR2 form can start
directly at the MakerNote value. These Sony handlers do not introduce a new
offset base, so out-of-line value offsets remain relative to the enclosing
TIFF base. Do not assume that all camera MakerNotes share those semantics.
[ExifTool MakerNote dispatch](https://github.com/exiftool/exiftool/blob/master/lib/Image/ExifTool/MakerNotes.pm)

## Validation boundary

Use the local originals for end-to-end extraction and JPEG-decoder timing.
Compare every retained candidate against an independent metadata reader and
inspect the selected image. Replay the recorded structures as sparse synthetic
TIFFs for deterministic traversal/selection tests; label generated JPEG bytes
as synthetic. Neither those tests nor a six-file public metadata corpus proves
support for every Sony model or firmware.

Before claiming broad A7 V support, include all six compression/crop modes,
portrait orientation, several camera firmware versions where available,
RAW-only versus RAW+JPEG captures, damaged/truncated copies, and unknown
optional tags. Real 2,000-file navigation and thermal behavior still require
the later asynchronous GUI and a representative Intel Mac workload.
