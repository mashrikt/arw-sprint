//! Metadata-derived Sony A7 V regression layouts. No photograph bytes are used.
//!
//! Constants are transcribed from fixtures/sony-a7v-{public,local}-layouts.json;
//! provenance and verification limits are recorded there and in
//! docs/sample-layouts.md. JPEG payloads below are synthetic marker framing,
//! not entropy-decodable images. Their declared ranges match the real layouts.

use fastcull::arw::PreviewReader;
use std::{
    cell::Cell,
    io::{self, Read, Seek, SeekFrom},
    ops::Range,
    rc::Rc,
};

#[derive(Clone, Copy)]
struct Layout {
    name: &'static str,
    ifd1: u32,
    ifd2: u32,
    hd_length: u32,
    thumbnail_offset: u32,
    thumbnail_length: u32,
    best_offset: u32,
    best_length: u32,
    best_dimensions: (u16, u16),
    raw_offset: u32,
    raw_length: u32,
    raw_dimensions: (u32, u32),
    file_length: Option<u64>,
}

// The public EXIF listings identify IFD0/IFD1/IFD2 and the RAW SubIFD but
// do not expose every IFD table's physical address. Only those public table
// addresses are synthetic; JPEG and RAW ranges are the published values.
const PUBLIC_TABLES: (u32, u32) = (1024, 2048);
const LOCAL_IFD2: u32 = 145_074;
const RAW_IFD: u32 = 145_382;
const HD_OFFSET: u32 = 200_866;

const LAYOUTS: [Layout; 9] = [
    Layout {
        name: "public-8845-full-lossless",
        ifd1: PUBLIC_TABLES.0,
        ifd2: PUBLIC_TABLES.1,
        hd_length: 343_945,
        thumbnail_offset: 43_992,
        thumbnail_length: 6_445,
        best_offset: 548_864,
        best_length: 4_734_564,
        best_dimensions: (7008, 4672),
        raw_offset: 5_283_840,
        raw_length: 38_911_408,
        raw_dimensions: (7168, 5120),
        file_length: None,
    },
    Layout {
        name: "public-8846-full-hq",
        ifd1: PUBLIC_TABLES.0,
        ifd2: PUBLIC_TABLES.1,
        hd_length: 333_230,
        thumbnail_offset: 43_992,
        thumbnail_length: 6_339,
        best_offset: 536_576,
        best_length: 4_259_249,
        best_dimensions: (7008, 4672),
        raw_offset: 4_796_416,
        raw_length: 18_432_512,
        raw_dimensions: (7040, 4688),
        file_length: None,
    },
    Layout {
        name: "public-8847-full-compressed",
        ifd1: PUBLIC_TABLES.0,
        ifd2: PUBLIC_TABLES.1,
        hd_length: 333_372,
        thumbnail_offset: 43_992,
        thumbnail_length: 6_360,
        best_offset: 536_576,
        best_length: 4_270_440,
        best_dimensions: (7008, 4672),
        raw_offset: 4_808_704,
        raw_length: 18_504_704,
        raw_dimensions: (7040, 4688),
        file_length: None,
    },
    Layout {
        name: "public-8848-apsc-lossless",
        ifd1: PUBLIC_TABLES.0,
        ifd2: PUBLIC_TABLES.1,
        hd_length: 440_228,
        thumbnail_offset: 43_992,
        thumbnail_length: 7_374,
        best_offset: 643_072,
        best_length: 2_359_452,
        best_dimensions: (4608, 3072),
        raw_offset: 3_006_464,
        raw_length: 19_224_032,
        raw_dimensions: (5120, 3584),
        file_length: None,
    },
    Layout {
        name: "public-8849-apsc-hq",
        ifd1: PUBLIC_TABLES.0,
        ifd2: PUBLIC_TABLES.1,
        hd_length: 433_383,
        thumbnail_offset: 43_992,
        thumbnail_length: 7_329,
        best_offset: 634_880,
        best_length: 2_312_397,
        best_dimensions: (4608, 3072),
        raw_offset: 2_949_120,
        raw_length: 8_529_408,
        raw_dimensions: (4640, 3088),
        file_length: None,
    },
    Layout {
        name: "public-8850-apsc-compressed",
        ifd1: PUBLIC_TABLES.0,
        ifd2: PUBLIC_TABLES.1,
        hd_length: 452_187,
        thumbnail_offset: 43_992,
        thumbnail_length: 7_283,
        best_offset: 655_360,
        best_length: 2_555_667,
        best_dimensions: (4608, 3072),
        raw_offset: 3_211_264,
        raw_length: 8_890_880,
        raw_dimensions: (4640, 3088),
        file_length: None,
    },
    Layout {
        name: "private-local-1",
        ifd1: 43_690,
        ifd2: LOCAL_IFD2,
        hd_length: 260_685,
        thumbnail_offset: 43_964,
        thumbnail_length: 8_064,
        best_offset: 462_848,
        best_length: 1_170_107,
        best_dimensions: (4608, 3072),
        raw_offset: 1_634_304,
        raw_length: 17_981_296,
        raw_dimensions: (5120, 3584),
        file_length: Some(19_615_744),
    },
    Layout {
        name: "private-local-2",
        ifd1: 43_692,
        ifd2: LOCAL_IFD2,
        hd_length: 283_938,
        thumbnail_offset: 43_966,
        thumbnail_length: 6_450,
        best_offset: 487_424,
        best_length: 2_496_810,
        best_dimensions: (7008, 4672),
        raw_offset: 2_985_984,
        raw_length: 37_625_808,
        raw_dimensions: (7168, 5120),
        file_length: Some(40_611_840),
    },
    Layout {
        name: "private-local-3",
        ifd1: 43_692,
        ifd2: LOCAL_IFD2,
        hd_length: 185_384,
        thumbnail_offset: 43_966,
        thumbnail_length: 6_312,
        best_offset: 389_120,
        best_length: 4_513_637,
        best_dimensions: (7008, 4672),
        raw_offset: 4_902_912,
        raw_length: 41_744_032,
        raw_dimensions: (7168, 5120),
        file_length: Some(46_649_344),
    },
];

/// Holes produce zero bytes; only small directory tables and marker fragments
/// are stored. The virtual RAW region is inaccessible even to speculative reads.
struct SparseArw {
    position: u64,
    length: u64,
    fragments: Vec<(u64, Vec<u8>)>,
    raw: Range<u64>,
    forbidden_reads: Rc<Cell<usize>>,
}

impl Read for SparseArw {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        let n = (self.length.saturating_sub(self.position)).min(out.len() as u64) as usize;
        if n == 0 {
            return Ok(0);
        }
        let end = self.position + n as u64;
        if self.position < self.raw.end && end > self.raw.start {
            self.forbidden_reads.set(self.forbidden_reads.get() + 1);
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "preview discovery attempted to read RAW sensor data",
            ));
        }
        out[..n].fill(0);
        for (start, bytes) in &self.fragments {
            let first = self.position.max(*start);
            let last = end.min(*start + bytes.len() as u64);
            if first < last {
                let dst = (first - self.position) as usize;
                let src = (first - start) as usize;
                let count = (last - first) as usize;
                out[dst..dst + count].copy_from_slice(&bytes[src..src + count]);
            }
        }
        self.position = end;
        Ok(n)
    }
}

impl Seek for SparseArw {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let next = match from {
            SeekFrom::Start(n) => i128::from(n),
            SeekFrom::Current(n) => i128::from(self.position) + i128::from(n),
            SeekFrom::End(n) => i128::from(self.length) + i128::from(n),
        };
        self.position = u64::try_from(next)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid sparse seek"))?;
        Ok(self.position)
    }
}

fn ifd(entries: &[(u16, u32)], next: u32) -> Vec<u8> {
    let mut out = (entries.len() as u16).to_le_bytes().to_vec();
    for &(tag, value) in entries {
        out.extend_from_slice(&tag.to_le_bytes());
        out.extend_from_slice(&4u16.to_le_bytes()); // LONG, one inline value
        out.extend_from_slice(&1u32.to_le_bytes());
        out.extend_from_slice(&value.to_le_bytes());
    }
    out.extend_from_slice(&next.to_le_bytes());
    out
}

fn jpeg_fragments(offset: u32, length: u32, dimensions: (u16, u16)) -> [(u64, Vec<u8>); 2] {
    // SOI, baseline SOF (three components), then a structurally valid SOS.
    // The zero-filled virtual entropy payload is deliberately not a real image.
    let mut header = vec![0xff, 0xd8, 0xff, 0xc0, 0, 17, 8];
    header.extend_from_slice(&dimensions.1.to_be_bytes());
    header.extend_from_slice(&dimensions.0.to_be_bytes());
    header.extend_from_slice(&[
        3, 1, 0x11, 0, 2, 0x11, 0, 3, 0x11, 0, // SOF components
        0xff, 0xda, 0, 12, 3, 1, 0, 2, 0, 3, 0, 0, 63, 0, // SOS
    ]);
    [
        (u64::from(offset), header),
        (u64::from(offset) + u64::from(length) - 2, vec![0xff, 0xd9]),
    ]
}

fn sparse(layout: Layout) -> SparseArw {
    let raw =
        u64::from(layout.raw_offset)..u64::from(layout.raw_offset) + u64::from(layout.raw_length);
    let mut fragments = vec![
        (0, vec![b'I', b'I', 42, 0, 8, 0, 0, 0]),
        (
            8,
            ifd(
                &[
                    (0x00fe, 1),
                    (0x0103, 6),
                    (0x014a, RAW_IFD),
                    (0x0201, HD_OFFSET),
                    (0x0202, layout.hd_length),
                ],
                layout.ifd1,
            ),
        ),
        (
            u64::from(layout.ifd1),
            ifd(
                &[
                    (0x00fe, 1),
                    (0x0103, 6),
                    (0x0201, layout.thumbnail_offset),
                    (0x0202, layout.thumbnail_length),
                ],
                layout.ifd2,
            ),
        ),
        (
            u64::from(layout.ifd2),
            ifd(
                &[
                    (0x00fe, 1),
                    (0x0100, u32::from(layout.best_dimensions.0)),
                    (0x0101, u32::from(layout.best_dimensions.1)),
                    (0x0102, 8),
                    (0x0103, 7),
                    (0x0106, 6),
                    (0x0115, 3),
                    (0x0201, layout.best_offset),
                    (0x0202, layout.best_length),
                ],
                0,
            ),
        ),
        (
            u64::from(RAW_IFD),
            ifd(
                &[
                    (0x00fe, 0),
                    (0x0100, layout.raw_dimensions.0),
                    (0x0101, layout.raw_dimensions.1),
                    (0x0102, 14),
                    // Force the dangerous JPEG-compression case even for
                    // public HQ/compressed layouts whose RAW encoding differs.
                    // The lossless and local originals actually use this value.
                    (0x0103, 7),
                    (0x0106, 32803), // CFA, never a display preview
                    (0x0111, layout.raw_offset),
                    (0x0115, 1),
                    (0x0117, layout.raw_length),
                ],
                0,
            ),
        ),
    ];
    fragments.extend(jpeg_fragments(HD_OFFSET, layout.hd_length, (1616, 1080)));
    // Public metadata omits thumbnail SOF dimensions; 160x120 is synthetic
    // there and matches the independently inspected local thumbnail headers.
    fragments.extend(jpeg_fragments(
        layout.thumbnail_offset,
        layout.thumbnail_length,
        (160, 120),
    ));
    fragments.extend(jpeg_fragments(
        layout.best_offset,
        layout.best_length,
        layout.best_dimensions,
    ));
    fragments.sort_by_key(|(offset, _)| *offset);
    assert!(fragments
        .windows(2)
        .all(|pair| pair[0].0 + pair[0].1.len() as u64 <= pair[1].0));
    assert!(fragments
        .iter()
        .all(|(offset, bytes)| { offset + bytes.len() as u64 <= raw.start }));
    assert!(
        fragments
            .iter()
            .map(|(_, bytes)| bytes.len())
            .sum::<usize>()
            < 1024
    );
    SparseArw {
        position: 0,
        length: layout.file_length.unwrap_or(raw.end),
        fragments,
        raw,
        forbidden_reads: Rc::new(Cell::new(0)),
    }
}

#[test]
fn nine_observed_a7v_layouts_choose_ifd2_without_reading_sensor_data() {
    for layout in LAYOUTS {
        let source = sparse(layout);
        let forbidden = Rc::clone(&source.forbidden_reads);
        let length = source.length;
        let mut reader = PreviewReader::from_reader(source, length);
        let best = reader
            .find_best_preview()
            .unwrap_or_else(|error| panic!("{}: {error}", layout.name));
        assert_eq!(
            best.offset,
            u64::from(layout.best_offset),
            "{}",
            layout.name
        );
        assert_eq!(
            best.length,
            u64::from(layout.best_length),
            "{}",
            layout.name
        );
        assert_eq!(
            best.width,
            Some(u32::from(layout.best_dimensions.0)),
            "{}",
            layout.name
        );
        assert_eq!(
            best.height,
            Some(u32::from(layout.best_dimensions.1)),
            "{}",
            layout.name
        );
        assert_eq!(forbidden.get(), 0, "{} read CFA data", layout.name);
        assert!(
            reader.warnings().is_empty(),
            "{}: {:?}",
            layout.name,
            reader.warnings()
        );
        assert!(
            reader.io_stats().bytes_read < 40 * 1024,
            "{} read {} bytes",
            layout.name,
            reader.io_stats().bytes_read
        );
    }
}
