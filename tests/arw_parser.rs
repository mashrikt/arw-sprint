//! Synthetic container fixtures, not camera-compatibility claims. JPEG entropy
//! here is framing-test data; actual decoder tests use a separate valid JPEG.
use fastcull::arw::{EmbeddedPreview, PreviewReader};
use std::io::{self, Cursor, Read, Seek, SeekFrom};

#[derive(Clone, Copy)]
enum ByteOrder {
    Little,
    Big,
}

impl ByteOrder {
    fn u16(self, value: u16) -> [u8; 2] {
        match self {
            Self::Little => value.to_le_bytes(),
            Self::Big => value.to_be_bytes(),
        }
    }

    fn u32(self, value: u32) -> [u8; 4] {
        match self {
            Self::Little => value.to_le_bytes(),
            Self::Big => value.to_be_bytes(),
        }
    }
}

#[derive(Clone, Copy)]
struct Tag {
    tag: u16,
    kind: u16,
    count: u32,
    value: u32,
}

fn long(tag: u16, value: u32) -> Tag {
    Tag {
        tag,
        kind: 4,
        count: 1,
        value,
    }
}

fn short(tag: u16, value: u16) -> Tag {
    Tag {
        tag,
        kind: 3,
        count: 1,
        value: value.into(),
    }
}

struct Fixture {
    bytes: Vec<u8>,
    order: ByteOrder,
}

impl Fixture {
    fn new(order: ByteOrder) -> Self {
        let mut fixture = Self {
            bytes: vec![0; 8],
            order,
        };
        fixture.bytes[..2].copy_from_slice(match order {
            ByteOrder::Little => b"II",
            ByteOrder::Big => b"MM",
        });
        fixture.bytes[2..4].copy_from_slice(&order.u16(42));
        fixture.bytes[4..8].copy_from_slice(&order.u32(8));
        fixture
    }

    fn put(&mut self, offset: usize, data: &[u8]) {
        self.bytes
            .resize(self.bytes.len().max(offset + data.len()), 0);
        self.bytes[offset..offset + data.len()].copy_from_slice(data);
    }

    fn ifd(&mut self, offset: usize, tags: &[Tag], next: Option<u32>) {
        self.put(offset, &self.order.u16(tags.len() as u16));
        for (index, tag) in tags.iter().enumerate() {
            let start = offset + 2 + index * 12;
            self.put(start, &self.order.u16(tag.tag));
            self.put(start + 2, &self.order.u16(tag.kind));
            self.put(start + 4, &self.order.u32(tag.count));
            let value = if tag.kind == 3 && tag.count == 1 {
                let bytes = self.order.u16(tag.value as u16);
                [bytes[0], bytes[1], 0, 0]
            } else {
                self.order.u32(tag.value)
            };
            self.put(start + 8, &value);
        }
        if let Some(next) = next {
            self.put(offset + 2 + tags.len() * 12, &self.order.u32(next));
        }
    }

    fn reader(self) -> PreviewReader<Cursor<Vec<u8>>> {
        let len = self.bytes.len() as u64;
        PreviewReader::from_reader(Cursor::new(self.bytes), len)
    }
}

fn jpeg(width: u16, height: u16, extra_entropy: usize) -> Vec<u8> {
    let mut jpeg = vec![0xff, 0xd8, 0xff, 0xc0, 0, 11, 8];
    jpeg.extend_from_slice(&height.to_be_bytes());
    jpeg.extend_from_slice(&width.to_be_bytes());
    jpeg.extend_from_slice(&[1, 1, 0x11, 0, 0xff, 0xda, 0, 8, 1, 1, 0, 0, 63, 0]);
    jpeg.resize(jpeg.len() + extra_entropy, 0x23);
    jpeg.extend_from_slice(&[0x12, 0xff, 0, 0x45, 0xff, 0xd0, 0x67, 0xff, 0xd9]);
    jpeg
}

fn preview_tags(offset: u32, jpeg: &[u8]) -> [Tag; 2] {
    [long(0x0201, offset), long(0x0202, jpeg.len() as u32)]
}

fn basic(order: ByteOrder, jpeg: &[u8]) -> Fixture {
    let mut fixture = Fixture::new(order);
    fixture.ifd(8, &preview_tags(512, jpeg), Some(0));
    fixture.put(512, jpeg);
    fixture
}

#[test]
fn discovers_and_extracts_in_both_byte_orders() {
    let jpeg = jpeg(7008, 4672, 12);
    for order in [ByteOrder::Little, ByteOrder::Big] {
        let mut reader = basic(order, &jpeg).reader();
        let preview = reader.find_best_preview().unwrap();
        assert_eq!((preview.offset, preview.length), (512, jpeg.len() as u64));
        assert_eq!((preview.width, preview.height), (Some(7008), Some(4672)));
        let mut output = vec![99; 1000];
        reader.read_preview_into(&preview, &mut output).unwrap();
        assert_eq!(output, jpeg);
        assert!(reader.io_stats().bytes_read < 1000);
        assert!(reader.warnings().is_empty());
    }
}

#[test]
fn walks_next_ifds_and_subifd_arrays_and_selects_pixels_before_byte_length() {
    let thumbnail = jpeg(160, 120, 3000);
    let largest = jpeg(7008, 4672, 0);
    let medium = jpeg(6000, 4000, 100);
    let largest_more_bytes = jpeg(7008, 4672, 50);
    for order in [ByteOrder::Little, ByteOrder::Big] {
        let mut fixture = Fixture::new(order);
        let mut root = preview_tags(4096, &thumbnail).to_vec();
        root.extend_from_slice(&[
            Tag {
                tag: 0x014a,
                kind: 4,
                count: 2,
                value: 192,
            },
            long(0x0100, u32::MAX), // Deliberately misleading RAW sensor dimensions.
            long(0x0101, u32::MAX),
        ]);
        fixture.ifd(8, &root, Some(768));
        fixture.put(192, &order.u32(256));
        fixture.put(196, &order.u32(512));
        fixture.ifd(256, &preview_tags(8192, &largest), Some(0));
        fixture.ifd(512, &preview_tags(12288, &medium), Some(0));
        fixture.ifd(768, &preview_tags(16384, &largest_more_bytes), Some(0));
        for (offset, bytes) in [
            (4096, &thumbnail),
            (8192, &largest),
            (12288, &medium),
            (16384, &largest_more_bytes),
        ] {
            fixture.put(offset, bytes);
        }
        let previews = fixture.reader().find_previews().unwrap();
        assert_eq!(
            previews
                .iter()
                .map(|preview| preview.offset)
                .collect::<Vec<_>>(),
            [16384, 8192, 12288, 4096]
        );
    }
}

#[test]
fn corrupt_candidate_does_not_hide_an_independent_valid_ifd() {
    let valid = jpeg(80, 60, 0);
    let mut corrupt = jpeg(7008, 4672, 0);
    corrupt[0] = 0;
    let mut fixture = basic(ByteOrder::Little, &corrupt);
    fixture.ifd(8, &preview_tags(512, &corrupt), Some(128));
    fixture.ifd(128, &preview_tags(1024, &valid), Some(0));
    fixture.put(1024, &valid);
    let mut reader = fixture.reader();
    assert_eq!(reader.find_best_preview().unwrap().offset, 1024);
    assert!(!reader.warnings().is_empty());
}

#[test]
fn skips_invalid_ranges_and_reports_no_preview_without_panicking() {
    let valid = jpeg(80, 60, 0);
    for tags in [
        vec![long(0x0201, u32::MAX), long(0x0202, 1024)],
        vec![long(0x0201, 512), long(0x0202, u32::MAX)],
        vec![long(0x0201, 512), long(0x0202, 0)],
        vec![Tag {
            tag: 0x014a,
            kind: 4,
            count: u32::MAX,
            value: u32::MAX,
        }],
        vec![long(0x8769, u32::MAX)],
    ] {
        let mut fixture = basic(ByteOrder::Little, &valid);
        fixture.ifd(8, &tags, Some(0));
        assert!(fixture.reader().find_best_preview().is_err());
    }
    for first in [0, 1, 7, u32::MAX] {
        let mut fixture = basic(ByteOrder::Little, &valid);
        fixture.put(4, &first.to_le_bytes());
        assert!(fixture.reader().find_best_preview().is_err());
    }
}

#[test]
fn ignores_enormous_unknown_entries_instead_of_allocating_or_following_them() {
    let valid = jpeg(80, 60, 0);
    let mut fixture = basic(ByteOrder::Little, &valid);
    let mut tags = preview_tags(512, &valid).to_vec();
    tags.push(Tag {
        tag: 0xfefe,
        kind: 12,
        count: u32::MAX,
        value: u32::MAX,
    });
    fixture.ifd(8, &tags, Some(0));
    let mut reader = fixture.reader();
    assert!(reader.find_best_preview().is_ok());
    assert!(reader.io_stats().bytes_read < 1000);
}

#[test]
fn rejects_excessive_entry_counts_and_truncated_tables() {
    for count in [4097, u16::MAX] {
        let mut fixture = Fixture::new(ByteOrder::Little);
        fixture.put(8, &count.to_le_bytes());
        let mut reader = fixture.reader();
        assert!(reader.find_best_preview().is_err());
        assert!(reader.io_stats().bytes_read <= 10);
    }
    for length in 0..38 {
        let fixture = basic(ByteOrder::Little, &jpeg(80, 60, 0));
        let mut reader = PreviewReader::from_reader(
            Cursor::new(fixture.bytes[..length].to_vec()),
            length as u64,
        );
        assert!(reader.find_best_preview().is_err(), "truncation {length}");
    }
}

#[test]
fn ifd_cycles_and_duplicate_candidates_are_bounded() {
    let valid = jpeg(80, 60, 0);
    let mut fixture = basic(ByteOrder::Little, &valid);
    let mut tags = preview_tags(512, &valid).to_vec();
    tags.push(long(0x014a, 128));
    fixture.ifd(8, &tags, Some(8));
    let mut child = preview_tags(512, &valid).to_vec();
    child.push(long(0x014a, 8));
    fixture.ifd(128, &child, Some(128));
    let mut reader = fixture.reader();
    assert_eq!(reader.find_previews().unwrap().len(), 1);
    assert!(reader.io_stats().read_calls < 12);
    assert!(reader.io_stats().bytes_read < 1000);
}

#[test]
fn sony_preview_prefix_is_relative_to_tiff_and_headers_are_optional() {
    let valid = jpeg(7008, 4672, 0);
    for order in [ByteOrder::Little, ByteOrder::Big] {
        for header in [&b""[..], &b"SONY DSC \0\0\0"[..], &b"SONY CAM \0\0\0"[..]] {
            let mut fixture = Fixture::new(order);
            fixture.ifd(8, &[long(0x8769, 256)], Some(0));
            fixture.ifd(
                256,
                &[Tag {
                    tag: 0x927c,
                    kind: 7,
                    count: (header.len() + 14) as u32,
                    value: 512,
                }],
                Some(0),
            );
            fixture.put(512, header);
            // MakerNote ends immediately after its entry, without a next pointer.
            fixture.ifd(
                512 + header.len(),
                &[Tag {
                    tag: 0x2001,
                    kind: 7,
                    count: (valid.len() + 32) as u32,
                    value: 4096,
                }],
                None,
            );
            fixture.put(4096, &[0x55; 32]);
            fixture.put(4128, &valid);
            let mut reader = fixture.reader();
            let preview = reader.find_best_preview().unwrap();
            assert_eq!((preview.offset, preview.length), (4128, valid.len() as u64));
            let mut output = Vec::new();
            reader.read_preview_into(&preview, &mut output).unwrap();
            assert_eq!(output, valid);
        }
    }
}

#[test]
fn missing_jpeg_boundaries_and_invalid_dimensions_are_rejected() {
    let valid = jpeg(80, 60, 0);
    let mut missing_soi = valid.clone();
    missing_soi[0] = 0;
    let mut missing_eoi = valid.clone();
    *missing_eoi.last_mut().unwrap() = 0;
    let mut zero_width = valid.clone();
    zero_width[9..11].fill(0);
    let mut short_segment = valid.clone();
    short_segment[4..6].copy_from_slice(&1u16.to_be_bytes());
    let mut trailing_garbage = valid.clone();
    trailing_garbage.extend_from_slice(&[12, 34]);
    for corrupt in [
        missing_soi,
        missing_eoi,
        zero_width,
        short_segment,
        trailing_garbage,
    ] {
        assert!(basic(ByteOrder::Little, &corrupt)
            .reader()
            .find_best_preview()
            .is_err());
    }
}

#[test]
fn extraction_trims_padding_and_checks_complete_jpeg_framing() {
    let valid = jpeg(80, 60, 0);
    let mut padded = valid.clone();
    padded.extend_from_slice(&[0, 0xff, 0, 0xff]);
    let mut reader = basic(ByteOrder::Little, &padded).reader();
    let preview = reader.find_best_preview().unwrap();
    assert_eq!(preview.length, valid.len() as u64);
    let mut output = Vec::new();
    reader.read_preview_into(&preview, &mut output).unwrap();
    assert_eq!(output, valid);

    let mut broken_scan = valid;
    broken_scan[19] = 0; // SOS component count becomes zero; SOF/EOI are intact.
    let mut reader = basic(ByteOrder::Little, &broken_scan).reader();
    let preview = reader.find_best_preview().unwrap();
    assert!(reader.read_preview_into(&preview, &mut output).is_err());
    assert!(output.is_empty());
}

#[test]
fn caller_supplied_preview_ranges_cannot_overflow_or_allocate_unboundedly() {
    let mut reader = basic(ByteOrder::Little, &jpeg(80, 60, 0)).reader();
    for (offset, length) in [
        (u64::MAX, 4),
        (u64::MAX - 3, 8),
        (0, u64::MAX),
        (512, 1),
        (0, 129 * 1024 * 1024),
    ] {
        let mut output = vec![42; 12];
        let preview = EmbeddedPreview {
            offset,
            length,
            width: None,
            height: None,
        };
        assert!(reader.read_preview_into(&preview, &mut output).is_err());
        assert!(output.is_empty());
        assert!(output.capacity() < 1024);
    }
}

struct SparseGuard {
    metadata: Vec<u8>,
    position: u64,
    file_len: u64,
}

impl Read for SparseGuard {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        let end = self.position.checked_add(output.len() as u64).unwrap();
        assert!(
            end <= self.metadata.len() as u64,
            "attempted read of RAW payload at {}..{end}",
            self.position
        );
        output.copy_from_slice(&self.metadata[self.position as usize..end as usize]);
        self.position = end;
        Ok(output.len())
    }
}

impl Seek for SparseGuard {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let position = match from {
            SeekFrom::Start(position) => i128::from(position),
            SeekFrom::Current(delta) => i128::from(self.position) + i128::from(delta),
            SeekFrom::End(delta) => i128::from(self.file_len) + i128::from(delta),
        };
        self.position = u64::try_from(position)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid seek"))?;
        Ok(self.position)
    }
}

#[test]
fn sparse_multi_gigabyte_arw_never_reads_raw_cfa_strip_payload() {
    let valid = jpeg(7008, 4672, 0);
    let mut fixture = Fixture::new(ByteOrder::Little);
    let mut tags = preview_tags(4096, &valid).to_vec();
    tags.extend_from_slice(&[
        short(0x0103, 7),     // RAW can use JPEG compression too.
        short(0x0106, 32803), // CFA photometric interpretation.
        short(0x0102, 14),
        short(0x0115, 1),
        long(0x0111, 1_000_000),
        long(0x0117, 64 * 1024 * 1024),
    ]);
    fixture.ifd(8, &tags, Some(0));
    fixture.put(4096, &valid);
    let file_len = 8 * 1024 * 1024 * 1024;
    let source = SparseGuard {
        metadata: fixture.bytes,
        position: 0,
        file_len,
    };
    let mut reader = PreviewReader::from_reader(source, file_len);
    let preview = reader.find_best_preview().unwrap();
    let mut output = Vec::new();
    reader.read_preview_into(&preview, &mut output).unwrap();
    assert_eq!(output, valid);
    assert!(reader.io_stats().bytes_read < 4096);
}

#[test]
fn jpeg_strip_previews_require_a_non_raw_photometric_interpretation() {
    let valid = jpeg(640, 480, 0);
    for (photometric, bits, expect_preview) in [
        (2, 8, true),
        (6, 8, true),
        (32803, 8, false),
        (2, 14, false),
    ] {
        let mut fixture = Fixture::new(ByteOrder::Big);
        fixture.ifd(
            8,
            &[
                short(0x0103, 7),
                short(0x0106, photometric),
                short(0x0102, bits),
                short(0x0115, 3),
                long(0x0111, 512),
                long(0x0117, valid.len() as u32),
            ],
            Some(0),
        );
        fixture.put(512, &valid);
        assert_eq!(fixture.reader().find_best_preview().is_ok(), expect_preview);
    }
}

#[test]
fn benchmark_cli_falls_back_after_complete_jpeg_framing_failure() {
    let mut corrupt_large = jpeg(7008, 4672, 0);
    corrupt_large[19] = 0;
    let valid_small = jpeg(80, 60, 0);
    let mut fixture = basic(ByteOrder::Little, &corrupt_large);
    fixture.ifd(8, &preview_tags(512, &corrupt_large), Some(128));
    fixture.ifd(128, &preview_tags(1024, &valid_small), Some(0));
    fixture.put(1024, &valid_small);

    struct TempDir(std::path::PathBuf);
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let directory = TempDir(std::env::temp_dir().join(format!(
        "fastcull-parser-test-{}-{nonce}",
        std::process::id()
    )));
    std::fs::create_dir(&directory.0).unwrap();
    let path = directory.0.join("corrupt-largest.ARW");
    std::fs::write(&path, fixture.bytes).unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_fastcull-bench"))
        .arg(&path)
        .args(["--order", "sequential"])
        .env("FASTCULL_LOG", "1")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("succeeded: 1; failed: 0"));
    assert!(String::from_utf8_lossy(&output.stderr).contains("fallback candidates skipped: 1"));
}

#[test]
fn deterministic_mutation_corpus_does_not_panic_or_read_beyond_bounds() {
    let original = basic(ByteOrder::Little, &jpeg(80, 60, 0)).bytes;
    let mut state = 0x5eed_cafe_u64;
    for case in 0..2048 {
        let mut bytes = original.clone();
        for _ in 0..1 + case % 8 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let index = state as usize % bytes.len();
            bytes[index] = (state >> 32) as u8;
        }
        if case % 4 == 0 {
            bytes.truncate(state as usize % bytes.len());
        }
        let file_len = bytes.len() as u64;
        let mut reader = PreviewReader::from_reader(Cursor::new(bytes), file_len);
        if let Ok(previews) = reader.find_previews() {
            for preview in previews {
                assert!(preview.offset.checked_add(preview.length).unwrap() <= file_len);
                let mut output = Vec::new();
                let _ = reader.read_preview_into(&preview, &mut output);
                assert!(output.capacity() <= 128 * 1024 * 1024);
            }
        }
        assert!(reader.io_stats().bytes_read < 8 * 1024 * 1024);
    }
}
